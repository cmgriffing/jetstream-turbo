//! Fast envelope parser.
//!
//! Extracts the message's top-level fields and commit metadata directly from
//! the wire, avoiding the simd-json tape. It strictly validates a narrow subset
//! (object structure, unescaped strings, u64 numbers, known keys) and returns
//! `None` for anything it does not fully handle — the caller then falls back to
//! the tape parser, so correctness is bounded by the tape. The returned tuple
//! is `(message, record_span)` where the record's raw wire span is captured for
//! lazy materialization.

use super::jetstream::{CommitData, JetstreamMessage, MessageKind, OperationType};

pub fn parse_envelope_fast(wire: &str) -> Option<(JetstreamMessage, Option<(usize, usize)>)> {
    let b = wire.as_bytes();

    #[inline(always)]
    fn ws(b: &[u8], mut i: usize) -> Option<usize> {
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        Some(i)
    }

    // Read an unescaped string starting at b[i] == '"'; returns (content, next).
    #[inline(always)]
    fn read_unescaped(b: &[u8], i: usize) -> Option<(&str, usize)> {
        if b.get(i)? != &b'"' {
            return None;
        }
        let start = i + 1;
        let mut j = i + 1;
        while j < b.len() && b[j] != b'"' {
            if b[j] == b'\\' {
                return None; // escapes: fall back to the tape parser
            }
            j += 1;
        }
        if j >= b.len() {
            return None;
        }
        Some((std::str::from_utf8(&b[start..j]).ok()?, j + 1))
    }

    // Read a u64 (digits only) starting at `i`; returns (value, next).
    #[inline(always)]
    fn read_u64(b: &[u8], i: usize) -> Option<(u64, usize)> {
        let start = i;
        let mut j = i;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == start {
            return None;
        }
        Some((std::str::from_utf8(&b[start..j]).ok()?.parse().ok()?, j))
    }

    // Skip any JSON value starting at `i`; returns (start, end). Used for the
    // record, which may contain escapes.
    #[inline(always)]
    fn skip_value(b: &[u8], i0: usize) -> Option<(usize, usize)> {
        let mut i = i0;
        if i >= b.len() {
            return None;
        }
        match b[i] {
            b'{' | b'[' => {
                let open = b[i];
                let close = if open == b'{' { b'}' } else { b']' };
                let mut depth = 1usize;
                i += 1;
                while i < b.len() && depth > 0 {
                    match b[i] {
                        b'"' => {
                            i += 1;
                            while i < b.len() && b[i] != b'"' {
                                if b[i] == b'\\' {
                                    i += 1;
                                }
                                i += 1;
                            }
                            i += 1;
                        }
                        c if c == open => {
                            depth += 1;
                            i += 1;
                        }
                        c if c == close => {
                            depth -= 1;
                            i += 1;
                        }
                        _ => i += 1,
                    }
                }
                if depth != 0 {
                    return None;
                }
                Some((i0, i))
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i >= b.len() {
                    return None;
                }
                Some((i0, i + 1))
            }
            _ => {
                while i < b.len()
                    && !matches!(b[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
                {
                    i += 1;
                }
                if i == i0 {
                    return None;
                }
                Some((i0, i))
            }
        }
    }

    // Parse the commit object starting at `i0` (points at '{').
    type CommitAndSpan = (Box<CommitData>, Option<(usize, usize)>);
    type ParsedCommit = (Box<CommitData>, Option<(usize, usize)>, usize);

    fn parse_commit(b: &[u8], i0: usize) -> Option<ParsedCommit> {
        let mut i = i0;
        if b.get(i) != Some(&b'{') {
            return None;
        }
        i += 1;
        let mut rev: Option<String> = None;
        let mut operation: Option<OperationType> = None;
        let mut collection: Option<String> = None;
        let mut rkey: Option<String> = None;
        let mut record: Option<(usize, usize)> = None;
        let mut cid: Option<String> = None;
        loop {
            i = ws(b, i)?;
            if i >= b.len() {
                return None;
            }
            if b[i] == b'}' {
                break;
            }
            if b[i] != b'"' {
                return None;
            }
            let (key, ni) = read_unescaped(b, i)?;
            i = ws(b, ni)?;
            if i >= b.len() || b[i] != b':' {
                return None;
            }
            i = ws(b, i + 1)?;
            match key {
                "rev" => {
                    let (v, ni2) = read_unescaped(b, i)?;
                    rev = Some(v.to_string());
                    i = ni2;
                }
                "operation" => {
                    let (v, ni2) = read_unescaped(b, i)?;
                    operation = Some(match v {
                        "create" => OperationType::Create,
                        "update" => OperationType::Update,
                        "delete" => OperationType::Delete,
                        _ => OperationType::Unknown,
                    });
                    i = ni2;
                }
                "collection" => {
                    let (v, ni2) = read_unescaped(b, i)?;
                    collection = Some(v.to_string());
                    i = ni2;
                }
                "rkey" => {
                    let (v, ni2) = read_unescaped(b, i)?;
                    rkey = Some(v.to_string());
                    i = ni2;
                }
                "cid" => {
                    let (v, ni2) = match read_unescaped(b, i) {
                        Some(v) => v,
                        None => {
                            return None;
                        }
                    };
                    cid = Some(v.to_string());
                    i = ni2;
                }
                "record" => {
                    if b.get(i) != Some(&b'{') {
                        return None; // only object records are handled
                    }
                    let (start, end) = skip_value(b, i)?;
                    record = Some((start, end));
                    i = end;
                }
                _ => return None, // unknown commit key: fall back to the tape
            }
            i = ws(b, i)?;
            if i >= b.len() {
                return None;
            }
            if b[i] == b'}' {
                break;
            }
            if b[i] != b',' {
                return None;
            }
            i += 1;
        }
        let commit = CommitData {
            rev: rev.map(Into::into),
            operation_type: operation?,
            collection: collection.map(Into::into),
            rkey: rkey.map(Into::into),
            record: None,
            cid,
        };
        Some((Box::new(commit), record, i + 1))
    }

    let mut i = ws(b, 0)?;
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    let mut did: Option<String> = None;
    let mut time_us: Option<u64> = None;
    let mut seq: Option<u64> = None;
    let mut kind: Option<MessageKind> = None;
    let mut commit: Option<CommitAndSpan> = None;
    loop {
        i = ws(b, i)?;
        if i >= b.len() {
            return None;
        }
        if b[i] == b'}' {
            break;
        }
        if b[i] != b'"' {
            return None;
        }
        let (key, ni) = read_unescaped(b, i)?;
        i = ws(b, ni)?;
        if i >= b.len() || b[i] != b':' {
            return None;
        }
        i = ws(b, i + 1)?;
        match key {
            "did" => {
                let (v, ni2) = read_unescaped(b, i)?;
                did = Some(v.to_string());
                i = ni2;
            }
            "time_us" => {
                let (v, ni2) = read_u64(b, i)?;
                time_us = Some(v);
                i = ni2;
            }
            "seq" => {
                let (v, ni2) = read_u64(b, i)?;
                seq = Some(v);
                i = ni2;
            }
            "kind" => {
                let (v, ni2) = read_unescaped(b, i)?;
                kind = Some(match v {
                    "commit" => MessageKind::Commit,
                    "identity" => MessageKind::Identity,
                    "account" => MessageKind::Account,
                    _ => MessageKind::Unknown,
                });
                i = ni2;
            }
            "commit" => {
                let (c, span, ni2) = parse_commit(b, i)?;
                commit = Some((c, span));
                i = ni2;
            }
            _ => return None, // unknown top-level key: fall back to the tape
        }
        i = ws(b, i)?;
        if i >= b.len() {
            return None;
        }
        if b[i] == b'}' {
            break;
        }
        if b[i] != b',' {
            return None;
        }
        i += 1;
    }
    let did = did?;
    let (commit_data, record_span) = match commit {
        Some((c, s)) => (Some(c), s),
        None => (None, None),
    };
    let message = JetstreamMessage {
        did: did.into(),
        time_us,
        seq,
        kind: kind.unwrap_or(MessageKind::Unknown),
        commit: commit_data,
        raw_json: None,
    };
    Some((message, record_span))
}

/// Fast path for the standard Jetstream wire shape (fixed field order:
/// did, time_us, seq, kind, commit{rev, operation, collection, rkey, record, cid},
/// no whitespace). Keys are verified with fixed compares instead of generic
/// string reads + matching. Returns `None` for any deviation (missing optional
/// fields, reordered keys, whitespace, escapes, unknown keys) — the caller then
/// falls back to the generic parser, so correctness is bounded.
pub fn parse_envelope_shape(wire: &str) -> Option<(JetstreamMessage, Option<(usize, usize)>)> {
    let b = wire.as_bytes();
    let mut i = 0usize;

    #[inline(always)]
    fn peek(b: &[u8], i: usize, pat: &[u8]) -> Option<usize> {
        if b.len() >= i + pat.len() && &b[i..i + pat.len()] == pat {
            Some(i + pat.len())
        } else {
            None
        }
    }

    // Read an unescaped string starting at b[i] == '"'; returns (content, next).
    #[inline(always)]
    fn read_unescaped(b: &[u8], i: usize) -> Option<(&str, usize)> {
        if b.get(i)? != &b'"' {
            return None;
        }
        let start = i + 1;
        let mut j = i + 1;
        while j < b.len() && b[j] != b'"' {
            if b[j] == b'\\' {
                return None;
            }
            j += 1;
        }
        if j >= b.len() {
            return None;
        }
        Some((std::str::from_utf8(&b[start..j]).ok()?, j + 1))
    }

    // Read a u64 (digits only) starting at `i`; returns (value, next).
    #[inline(always)]
    fn read_u64(b: &[u8], i: usize) -> Option<(u64, usize)> {
        let start = i;
        let mut j = i;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == start {
            return None;
        }
        Some((std::str::from_utf8(&b[start..j]).ok()?.parse().ok()?, j))
    }

    // Skip a JSON object starting at `i` (points at '{'); returns (start, end).
    #[inline(always)]
    fn skip_object(b: &[u8], i0: usize) -> Option<(usize, usize)> {
        let mut i = i0;
        if b.get(i)? != &b'{' {
            return None;
        }
        let mut depth = 1usize;
        i += 1;
        while i < b.len() && depth > 0 {
            match b[i] {
                b'"' => {
                    i += 1;
                    while i < b.len() && b[i] != b'"' {
                        if b[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    i += 1;
                }
                b'{' => {
                    depth += 1;
                    i += 1;
                }
                b'}' => {
                    depth -= 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        if depth != 0 {
            return None;
        }
        Some((i0, i))
    }

    // { "did" : value
    i = peek(b, i, b"{\"did\":")?;
    let (did, ni) = read_unescaped(b, i)?;
    i = ni;
    // , "time_us" : number
    i = peek(b, i, b",\"time_us\":")?;
    let (time_us, ni) = read_u64(b, i)?;
    i = ni;
    // , "seq" : number
    i = peek(b, i, b",\"seq\":")?;
    let (seq, ni) = read_u64(b, i)?;
    i = ni;
    // , "kind" : "commit"
    i = peek(b, i, b",\"kind\":\"commit\"")?;
    // , "commit" : {
    i = peek(b, i, b",\"commit\":{")?;
    // "rev" : value
    i = peek(b, i, b"\"rev\":")?;
    let (rev, ni) = read_unescaped(b, i)?;
    i = ni;
    // , "operation" : value
    i = peek(b, i, b",\"operation\":")?;
    let (op, ni) = read_unescaped(b, i)?;
    i = ni;
    let operation_type = match op {
        "create" => OperationType::Create,
        "update" => OperationType::Update,
        "delete" => OperationType::Delete,
        _ => OperationType::Unknown,
    };
    // , "collection" : value
    i = peek(b, i, b",\"collection\":")?;
    let (collection, ni) = read_unescaped(b, i)?;
    i = ni;
    // , "rkey" : value
    i = peek(b, i, b",\"rkey\":")?;
    let (rkey, ni) = read_unescaped(b, i)?;
    i = ni;
    // , "record" : { ... }
    i = peek(b, i, b",\"record\":")?;
    let (start, end) = skip_object(b, i)?;
    let record_span = Some((start, end));
    i = end;
    // , "cid" : value
    i = peek(b, i, b",\"cid\":")?;
    let (cid, ni) = read_unescaped(b, i)?;
    i = ni;
    // }}  (commit close, message close)
    peek(b, i, b"}}")?;

    let commit = Box::new(CommitData {
        rev: Some(rev.into()),
        operation_type,
        collection: Some(collection.into()),
        rkey: Some(rkey.into()),
        record: None,
        cid: Some(cid.into()),
    });
    let message = JetstreamMessage {
        did: did.into(),
        time_us: Some(time_us),
        seq: Some(seq),
        kind: MessageKind::Commit,
        commit: Some(commit),
        raw_json: None,
    };
    Some((message, record_span))
}

#[cfg(test)]
mod tests {
    use super::*;
    use simd_json::OwnedValue;

    fn tape_parse(wire: &str) -> JetstreamMessage {
        let mut owned = wire.to_string();
        let m: JetstreamMessage = unsafe { simd_json::from_str(&mut owned) }.expect("tape parse");
        m
    }

    fn assert_same(fast: JetstreamMessage, tape: JetstreamMessage) {
        assert_eq!(fast.did, tape.did);
        assert_eq!(fast.time_us, tape.time_us);
        assert_eq!(fast.seq, tape.seq);
        assert_eq!(fast.kind, tape.kind);
        match (fast.commit, tape.commit) {
            (None, None) => {}
            (Some(a), Some(b)) => {
                assert_eq!(a.rev, b.rev);
                assert_eq!(a.operation_type, b.operation_type);
                assert_eq!(a.collection, b.collection);
                assert_eq!(a.rkey, b.rkey);
                assert_eq!(a.cid, b.cid);
            }
            _ => panic!("commit presence mismatch"),
        }
    }

    #[test]
    fn fast_matches_tape_on_fixtures() {
        let wires = [
            r#"{"did":"did:plc:user0000","time_us":1770949213790196,"seq":100000,"kind":"commit","commit":{"rev":"3mepgzgimkv0000","operation":"create","collection":"app.bsky.feed.post","rkey":"3mepgzgia0000","record":{"$type":"app.bsky.feed.post","text":"hello"},"cid":"bafyreia0"}}"#,
            r#"{"did":"did:plc:alice","kind":"identity"}"#,
            r#"{"did":"did:plc:bob","kind":"account","seq":5}"#,
            r#"{"seq":5,"did":"did:plc:carol","kind":"commit","commit":{"operation":"delete","collection":"app.bsky.feed.like","rkey":"abc"}}"#,
            r#"{"did":"did:plc:dave","kind":"commit","commit":{"rev":"r","operation":"update","collection":"app.bsky.feed.post","rkey":"k","record":{"a":1,"nested":{"x":[1,2,"s"]}},"cid":"c"}}"#,
            r#"{"did":"did:plc:eve","time_us":5,"seq":6,"kind":"commit","commit":{"operation":"create","record":{"text":"hi"},"rev":"v","collection":"c","rkey":"k","cid":"cid"}}"#,
        ];
        for wire in wires {
            let (fast, span) = parse_envelope_fast(wire).expect("fast parse");
            let tape = tape_parse(wire);
            assert_same(fast, tape);
            // The record span must exactly capture the record value.
            if let Some((s, e)) = span {
                let _: OwnedValue = unsafe {
                    let mut owned = wire[s..e].to_string();
                    simd_json::from_str(&mut owned)
                }
                .expect("record span is valid JSON");
            }
        }
    }

    #[test]
    fn fast_falls_back_on_unsupported_structures() {
        let unsupported = [
            r#"{"did":"x","unknown":1,"kind":"commit"}"#,
            r#"{"did":"a\"b","kind":"identity"}"#,
            r#"{"did":"x","time_us":-5,"kind":"account"}"#,
            r#"{"did":"x","time_us":1.5,"kind":"account"}"#,
            r#"{ "did" "broken", "kind": "x" }"#,
            r#"{"did":"x","kind":"commit","commit":{"operation":"create","extra":{}}}"#,
            r#"{"did":"x","kind":"commit","commit":{"operation":"create","record":"not-an-object"}}"#,
            r#"{"did":"x","kind":"commit","commit":{"rev":"\"escaped\""}}"#,
        ];
        for wire in unsupported {
            assert!(
                parse_envelope_fast(wire).is_none(),
                "should fall back: {wire}"
            );
        }
    }

    #[test]
    fn fast_record_span_excludes_nothing_extra() {
        // Record is the FIRST commit field and the message has a trailing field.
        let wire = r#"{"did":"d","kind":"commit","commit":{"operation":"create","record":{"text":"hello","deep":{"a":["x",{"b":2}]}},"cid":"c"}}"#;
        let (_, span) = parse_envelope_fast(wire).unwrap();
        let (s, e) = span.expect("span");
        assert_eq!(
            &wire[s..e],
            r#"{"text":"hello","deep":{"a":["x",{"b":2}]}}"#
        );
    }

    #[test]
    fn shape_matches_generic_on_standard_wire() {
        // The benchmark fixture shape must parse identically via both paths.
        let batch = crate::testing::create_message_batch(50);
        for m in &batch {
            let wire = serde_json::to_string(m).unwrap();
            let (shape, shape_span) =
                parse_envelope_shape(&wire).unwrap_or_else(|| panic!("shape path failed"));
            let (generic, generic_span) =
                parse_envelope_fast(&wire).unwrap_or_else(|| panic!("generic path failed"));
            assert_eq!(shape.did, generic.did);
            assert_eq!(shape.time_us, generic.time_us);
            assert_eq!(shape.seq, generic.seq);
            assert_eq!(shape.kind, generic.kind);
            let sc = shape.commit.unwrap();
            let gc = generic.commit.unwrap();
            assert_eq!(sc.rev, gc.rev);
            assert_eq!(sc.operation_type, gc.operation_type);
            assert_eq!(sc.collection, gc.collection);
            assert_eq!(sc.rkey, gc.rkey);
            assert_eq!(sc.cid, gc.cid);
            assert_eq!(shape_span, generic_span);
        }
    }

    #[test]
    fn shape_falls_back_on_reordered_fields() {
        let wires = [
            r#"{"seq":5,"did":"d","kind":"commit","commit":{"operation":"create"}}"#,
            r#"{"did":"d","kind":"identity"}"#,
            r#"{"did":"d","time_us":1,"seq":2,"kind":"commit","commit":{"operation":"create","record":{},"rev":"r"}}"#,
            r#"{"did":"d","time_us":1,"seq":2,"kind":"commit","commit":{"operation":"create","record":{},"cid":"c"} , "extra":1}"#,
        ];
        for wire in wires {
            assert!(
                parse_envelope_shape(wire).is_none(),
                "shape should fall back: {wire}"
            );
        }
    }

    #[test]
    fn fast_parses_create_message_batch() {
        // The benchmark's fixture messages must all take the fast path.
        let batch = crate::testing::create_message_batch(50);
        for m in &batch {
            let wire = serde_json::to_string(m).unwrap();
            assert!(
                parse_envelope_fast(&wire).is_some(),
                "fixture should take the fast path"
            );
        }
    }
}
