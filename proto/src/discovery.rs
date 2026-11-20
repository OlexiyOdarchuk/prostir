//! Discovery by asking: the headset sends a query ([`Query`], `PRSQ`) to UDP port
//! [`crate::DISCOVERY_PORT`], as a broadcast and straight to the addresses of PCs it
//! knows (typed in, or the last one it reached, a VPN address too); a PC answers
//! ([`Answer`], `PRSA`) to the address and port the query came from, and only to it.
//!
//! Who a PC answers: everyone while it has no paired headset or while its pairing
//! window is open; otherwise only a query that carries its own tag ([`find_tag`]), the
//! tag a headset that knows the PC's key makes. The tag hides which PC is looked for
//! (the nonce is new each time), and the PC learns nothing of the other PCs the
//! headset knows.
//!
//! A query is at least [`MIN_QUERY`] bytes (padded with zeros) and an answer always
//! smaller ([`MAX_ANSWER`]): a forged sender address gets less from a PC than was
//! sent, so a PC cannot be used to amplify traffic.
//!
//! Neither is encrypted or authenticated beyond the tags: an answer is a hint where to
//! connect, and trust comes from the handshake, as with the beacon.

use sha2::{Digest, Sha256};

/// The start of a query.
pub const QUERY_MAGIC: &[u8; 4] = b"PRSQ";
/// The start of an answer.
pub const ANSWER_MAGIC: &[u8; 4] = b"PRSA";
/// The version of the format this build writes. A reader takes a query of a newer
/// version by the fields of this one: a newer version only adds after them.
pub const VERSION: u8 = 1;
/// The smallest query a PC reads.
pub const MIN_QUERY: usize = 128;
/// The biggest query a PC reads.
pub const MAX_QUERY: usize = 512;
/// The most PCs one query looks for.
pub const MAX_TAGS: usize = 32;
/// The longest headset name in a query, bytes.
pub const MAX_HEADSET_NAME: usize = 32;
/// The longest PC name in an answer, bytes.
pub const MAX_PC_NAME: usize = 64;
/// The biggest answer: always below [`MIN_QUERY`].
pub const MAX_ANSWER: usize = 4 + 1 + NONCE + TAG + 2 + 1 + 1 + MAX_PC_NAME;
/// The bytes of a nonce.
pub const NONCE: usize = 16;
/// The bytes of a tag.
pub const TAG: usize = 8;

const _: () = assert!(MAX_ANSWER < MIN_QUERY);
const _: () = assert!(4 + 1 + NONCE + 1 + MAX_TAGS * TAG + 1 + MAX_HEADSET_NAME <= MAX_QUERY);

/// Flags of an answer.
pub mod flags {
    /// The PC's pairing window is open: a new headset can pair now.
    pub const PAIRING: u8 = 1;
    /// The PC answers every query (no paired headset, or the pairing window is open).
    pub const ANSWERS_ALL: u8 = 2;
}

/// A tag of the PC whose key is `pc_key`, for a query with `nonce`: the headset puts
/// it in to look for that PC.
pub fn find_tag(pc_key: &[u8; 32], nonce: &[u8; NONCE]) -> [u8; TAG] {
    tag(b"prostir-find", pc_key, nonce)
}

/// The tag a PC puts in its answer to a query with `nonce`: the headset tells which of
/// the PCs it knows answered before it connects.
pub fn here_tag(pc_key: &[u8; 32], nonce: &[u8; NONCE]) -> [u8; TAG] {
    tag(b"prostir-here", pc_key, nonce)
}

fn tag(label: &[u8], pc_key: &[u8; 32], nonce: &[u8; NONCE]) -> [u8; TAG] {
    let mut h = Sha256::new();
    h.update(label);
    h.update(pc_key);
    h.update(nonce);
    let digest = h.finalize();
    let mut out = [0; TAG];
    out.copy_from_slice(&digest[..TAG]);
    out
}

/// `text` cut to at most `max` bytes, at a character's edge.
fn cut(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A headset asking which PCs are there.
///
/// ```text
/// 0     4  "PRSQ"
/// 4     1  version (1)
/// 5    16  nonce
/// 21    1  count (0 to 32)
/// 22  8×n  a tag for each PC looked for (find_tag)
/// .     1  name_len (0 to 32)
/// .     n  the headset's name, UTF-8: empty but while it looks for a PC to pair with
/// .     .  zeros up to 128 bytes at least, 512 at most
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub nonce: [u8; NONCE],
    pub tags: Vec<[u8; TAG]>,
    /// The headset's name, while it looks for a PC to pair with; empty otherwise.
    pub name: String,
}

impl Query {
    /// A query for the PCs whose keys are `pcs` (the first [`MAX_TAGS`]), with a new
    /// nonce; `name` is cut to [`MAX_HEADSET_NAME`] bytes.
    pub fn new<'a>(pcs: impl IntoIterator<Item = &'a [u8; 32]>, name: &str) -> Self {
        let mut nonce = [0; NONCE];
        getrandom::fill(&mut nonce).expect("system randomness");
        Self {
            nonce,
            tags: pcs
                .into_iter()
                .take(MAX_TAGS)
                .map(|key| find_tag(key, &nonce))
                .collect(),
            name: cut(name, MAX_HEADSET_NAME).to_owned(),
        }
    }

    /// Whether it looks for the PC whose key is `pc_key`.
    pub fn looks_for(&self, pc_key: &[u8; 32]) -> bool {
        let ours = find_tag(pc_key, &self.nonce);
        self.tags.contains(&ours)
    }

    /// The datagram: [`MIN_QUERY`] bytes at least.
    pub fn encode(&self) -> Vec<u8> {
        let tags = &self.tags[..self.tags.len().min(MAX_TAGS)];
        let name = cut(&self.name, MAX_HEADSET_NAME);
        let mut out = Vec::with_capacity(MIN_QUERY);
        out.extend_from_slice(QUERY_MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&self.nonce);
        out.push(tags.len() as u8);
        tags.iter().for_each(|t| out.extend_from_slice(t));
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        if out.len() < MIN_QUERY {
            out.resize(MIN_QUERY, 0);
        }
        out
    }

    /// Reads a datagram: `None` for anything that is not a whole query of
    /// [`MIN_QUERY`] to [`MAX_QUERY`] bytes (a beacon, a short or a broken one).
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if !(MIN_QUERY..=MAX_QUERY).contains(&buf.len()) {
            return None;
        }
        let rest = buf.strip_prefix(QUERY_MAGIC.as_slice())?;
        let (&version, rest) = rest.split_first()?;
        if version < 1 {
            return None;
        }
        let nonce: [u8; NONCE] = rest.get(..NONCE)?.try_into().ok()?;
        let (&count, mut rest) = rest[NONCE..].split_first()?;
        if usize::from(count) > MAX_TAGS {
            return None;
        }
        let mut tags = Vec::with_capacity(count.into());
        for _ in 0..count {
            tags.push(rest.get(..TAG)?.try_into().ok()?);
            rest = &rest[TAG..];
        }
        let (&len, rest) = rest.split_first()?;
        if usize::from(len) > MAX_HEADSET_NAME {
            return None;
        }
        let name = std::str::from_utf8(rest.get(..len.into())?)
            .ok()?
            .to_owned();
        Some(Self { nonce, tags, name })
    }
}

/// A PC answering a query.
///
/// ```text
/// 0     4  "PRSA"
/// 4     1  version (1)
/// 5    16  the query's nonce
/// 21    8  here_tag of the PC's key and the nonce
/// 29    2  the PC's TCP port, little-endian
/// 31    1  flags (1: pairing open, 2: answers every query)
/// 32    1  name_len (0 to 64)
/// 33    n  the PC's name, UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub nonce: [u8; NONCE],
    pub tag: [u8; TAG],
    pub port: u16,
    pub flags: u8,
    pub name: String,
}

impl Answer {
    /// The answer of the PC whose key is `pc_key` to `query`; `name` is cut to
    /// [`MAX_PC_NAME`] bytes.
    pub fn to(query: &Query, pc_key: &[u8; 32], port: u16, flags: u8, name: &str) -> Self {
        Self {
            nonce: query.nonce,
            tag: here_tag(pc_key, &query.nonce),
            port,
            flags,
            name: cut(name, MAX_PC_NAME).to_owned(),
        }
    }

    /// Whether it comes from the PC whose key is `pc_key`.
    pub fn from_pc(&self, pc_key: &[u8; 32]) -> bool {
        here_tag(pc_key, &self.nonce) == self.tag
    }

    /// The datagram: [`MAX_ANSWER`] bytes at most.
    pub fn encode(&self) -> Vec<u8> {
        let name = cut(&self.name, MAX_PC_NAME);
        let mut out = Vec::with_capacity(MAX_ANSWER);
        out.extend_from_slice(ANSWER_MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.tag);
        out.extend_from_slice(&self.port.to_le_bytes());
        out.push(self.flags);
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out
    }

    /// Reads a datagram: `None` for anything that is not an answer. Bytes after the
    /// name are a newer version's and are skipped.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        let rest = buf.strip_prefix(ANSWER_MAGIC.as_slice())?;
        let (&version, rest) = rest.split_first()?;
        if version < 1 {
            return None;
        }
        let nonce = rest.get(..NONCE)?.try_into().ok()?;
        let tag = rest.get(NONCE..NONCE + TAG)?.try_into().ok()?;
        let rest = &rest[NONCE + TAG..];
        let port = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?);
        let flags = *rest.get(2)?;
        let len = usize::from(*rest.get(3)?);
        if len > MAX_PC_NAME {
            return None;
        }
        let name = std::str::from_utf8(rest.get(4..4 + len)?).ok()?.to_owned();
        Some(Self {
            nonce,
            tag,
            port,
            flags,
            name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PC: [u8; 32] = [7; 32];
    const OTHER: [u8; 32] = [9; 32];

    #[test]
    fn a_query_reads_back_and_finds_its_pcs() {
        let q = Query::new([&PC, &OTHER], "Quest 3S");
        let bytes = q.encode();
        assert_eq!(bytes.len(), MIN_QUERY, "padded to the smallest size");
        assert_eq!(&bytes[..4], b"PRSQ");
        let back = Query::decode(&bytes).unwrap();
        assert_eq!(back, q);
        assert!(back.looks_for(&PC));
        assert!(back.looks_for(&OTHER));
        assert!(!back.looks_for(&[1; 32]));
        // A new nonce each time: the tags differ, so nobody tells which PC it is.
        let again = Query::new([&PC], "");
        assert_ne!(again.nonce, q.nonce);
        assert_ne!(again.tags[0], q.tags[0]);
    }

    #[test]
    fn a_query_with_every_tag_fits() {
        let keys: Vec<[u8; 32]> = (0..40u8).map(|i| [i; 32]).collect();
        let q = Query::new(keys.iter(), &"я".repeat(40));
        assert_eq!(q.tags.len(), MAX_TAGS, "at most 32 PCs");
        assert_eq!(
            q.name.len(),
            MAX_HEADSET_NAME,
            "the name cut at a character"
        );
        let bytes = q.encode();
        assert!(bytes.len() > MIN_QUERY && bytes.len() <= MAX_QUERY);
        assert_eq!(Query::decode(&bytes).unwrap(), q);
    }

    #[test]
    fn short_long_or_broken_queries_are_not_read() {
        let bytes = Query::new([&PC], "").encode();
        assert!(
            Query::decode(&bytes[..MIN_QUERY - 1]).is_none(),
            "127 bytes"
        );
        let mut long = bytes.clone();
        long.resize(MAX_QUERY + 1, 0);
        assert!(Query::decode(&long).is_none());
        long.truncate(MAX_QUERY);
        assert!(Query::decode(&long).is_some(), "padding up to 512 bytes");
        let mut magic = bytes.clone();
        magic[3] = b'A';
        assert!(Query::decode(&magic).is_none());
        let mut version = bytes.clone();
        version[4] = 0;
        assert!(Query::decode(&version).is_none());
        let mut count = bytes.clone();
        count[21] = 33;
        assert!(Query::decode(&count).is_none(), "more tags than allowed");
        let mut name = bytes.clone();
        name[22 + 8] = 33;
        assert!(Query::decode(&name).is_none(), "a name too long");
        let mut utf8 = bytes.clone();
        utf8[22 + 8] = 1;
        utf8[22 + 8 + 1] = 0xff;
        assert!(Query::decode(&utf8).is_none(), "a name that is not UTF-8");
        // The beacon is no query.
        let mut beacon = crate::encode_beacon(47830, "desk");
        beacon.resize(MIN_QUERY, 0);
        assert!(Query::decode(&beacon).is_none());
        // A newer version adds after the fields of this one.
        let mut newer = bytes.clone();
        newer[4] = 2;
        newer[60] = 0x55;
        assert_eq!(
            Query::decode(&newer).unwrap().tags,
            Query::decode(&bytes).unwrap().tags
        );
    }

    #[test]
    fn an_answer_reads_back_and_is_always_smaller_than_a_query() {
        let q = Query::new([&PC], "");
        let a = Answer::to(&q, &PC, 47830, flags::PAIRING, &"ж".repeat(100));
        assert_eq!(a.name.len(), MAX_PC_NAME);
        let bytes = a.encode();
        assert_eq!(bytes.len(), MAX_ANSWER);
        assert!(bytes.len() < MIN_QUERY);
        let back = Answer::decode(&bytes).unwrap();
        assert_eq!(back, a);
        assert!(back.from_pc(&PC));
        assert!(!back.from_pc(&OTHER));
        assert_eq!(back.nonce, q.nonce);
        // The tag of an answer is not the query's: a query cannot be sent back as one.
        assert_ne!(back.tag, q.tags[0]);
        assert!(Answer::decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(Answer::decode(&q.encode()).is_none());
        let mut more = bytes.clone();
        more.extend_from_slice(b"later");
        assert_eq!(Answer::decode(&more).unwrap(), a);
    }
}
