//! The X AIR protocol uses OSC strings, network-order numbers and little-endian
//! meter samples inside OSC blobs. No OSC bundles or wildcard addresses are used.

#[derive(Debug, PartialEq)]
pub(super) enum Arg<'a> {
    Int(i32),
    Float(f32),
    Text(&'a str),
    Blob(&'a [u8]),
}

fn padded(size: usize) -> usize {
    (size + 3) & !3
}

fn push_string(packet: &mut Vec<u8>, text: &str) {
    packet.extend_from_slice(text.as_bytes());
    packet.resize(padded(packet.len() + 1), 0);
}

pub(super) fn message(address: &str, args: &[Arg<'_>]) -> Vec<u8> {
    let mut packet = Vec::new();
    push_string(&mut packet, address);
    let mut tags = String::from(",");
    for arg in args {
        tags.push(match arg {
            Arg::Int(_) => 'i',
            Arg::Float(_) => 'f',
            Arg::Text(_) => 's',
            Arg::Blob(_) => 'b',
        });
    }
    push_string(&mut packet, &tags);
    for arg in args {
        match arg {
            Arg::Int(v) => packet.extend_from_slice(&v.to_be_bytes()),
            Arg::Float(v) => packet.extend_from_slice(&v.to_be_bytes()),
            Arg::Text(v) => push_string(&mut packet, v),
            Arg::Blob(v) => {
                packet.extend_from_slice(&(v.len() as u32).to_be_bytes());
                packet.extend_from_slice(v);
                packet.resize(padded(packet.len()), 0);
            }
        }
    }
    packet
}

fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    let (head, rest) = bytes.split_at_checked(n)?;
    *bytes = rest;
    Some(head)
}

fn string<'a>(bytes: &mut &'a [u8]) -> Option<&'a str> {
    let len = bytes.iter().position(|&c| c == 0)?;
    let raw = take(bytes, padded(len + 1))?;
    if raw[len..].iter().any(|&v| v != 0) {
        return None;
    }
    std::str::from_utf8(&raw[..len]).ok()
}

pub(super) fn parse(mut bytes: &[u8]) -> Option<(&str, Vec<Arg<'_>>)> {
    let address = string(&mut bytes)?;
    if !address.starts_with('/') || address.len() > 96 {
        return None;
    }
    let tags = string(&mut bytes)?.strip_prefix(',')?;
    if tags.len() > 16 {
        return None;
    }
    let mut args = Vec::with_capacity(tags.len());
    for tag in tags.bytes() {
        args.push(match tag {
            b'i' => Arg::Int(i32::from_be_bytes(take(&mut bytes, 4)?.try_into().ok()?)),
            b'f' => Arg::Float(f32::from_be_bytes(take(&mut bytes, 4)?.try_into().ok()?)),
            b's' => Arg::Text(string(&mut bytes)?),
            b'b' => {
                let size = u32::from_be_bytes(take(&mut bytes, 4)?.try_into().ok()?) as usize;
                if size > bytes.len() {
                    return None;
                }
                let raw = take(&mut bytes, padded(size))?;
                if raw[size..].iter().any(|&v| v != 0) {
                    return None;
                }
                Arg::Blob(&raw[..size])
            }
            _ => return None,
        });
    }
    bytes.is_empty().then_some((address, args))
}

pub(super) fn meter(blob: &[u8], count: usize) -> Option<Vec<f32>> {
    if blob.len() != 4 + 2 * count
        || u32::from_le_bytes(blob[..4].try_into().ok()?) as usize != count
    {
        return None;
    }
    Some(
        blob[4..]
            .chunks_exact(2)
            .map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 256.0)
            .collect(),
    )
}
