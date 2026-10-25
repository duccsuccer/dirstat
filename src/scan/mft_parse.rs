//! NTFS MFT file-record parser. Pure: takes one raw file record, returns the fields the tree needs.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MftRecord {
    pub index: u64,
    /// 48-bit parent record number, sequence number stripped.
    pub parent: u64,
    pub name: String,
    pub is_dir: bool,
    pub allocated: u64,
    pub logical: u64,
    /// True only when the record has a `$REPARSE_POINT` attribute whose tag is a name-surrogate
    /// (symlink `0xA000000C` or mount point `0xA0000003`). Other reparse tags (cloud placeholders,
    /// WOF, dedup) are false, so those files count as normal files.
    pub is_reparse: bool,
}

const RECORD_SECTOR: usize = 512;
const FLAG_IN_USE: u16 = 0x01;
const FLAG_DIRECTORY: u16 = 0x02;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;
const ATTR_HEADER_MIN: usize = 0x18;
const NS_DOS: u8 = 2;
const TAG_SYMLINK: u32 = 0xA000_000C;
const TAG_MOUNT_POINT: u32 = 0xA000_0003;
const PARENT_REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

struct FileName {
    parent: u64,
    namespace: u8,
    name: String,
}

/// Parses one raw file record. `index` is the caller's record number.
/// Returns `None` for unused, extension (non-base), corrupt or nameless records.
pub fn parse_record(buf: &mut [u8], index: u64) -> Option<MftRecord> {
    if buf.get(0..4)? != b"FILE" {
        return None;
    }
    apply_fixups(buf)?;
    let buf: &[u8] = buf;

    let flags = u16_at(buf, 0x16)?;
    if flags & FLAG_IN_USE == 0 || u64_at(buf, 0x20)? != 0 {
        return None;
    }

    let mut names = Vec::new();
    let mut data = None;
    let mut is_reparse = false;
    let mut off = u16_at(buf, 0x14)? as usize;
    loop {
        if u32_at(buf, off)? == ATTR_END {
            break;
        }
        let len = u32_at(buf, off + 4)? as usize;
        if len < ATTR_HEADER_MIN {
            return None;
        }
        let attr = buf.get(off..off.checked_add(len)?)?;
        let ty = u32_at(attr, 0)?;
        let non_resident = attr[8] != 0;
        if ty == ATTR_FILE_NAME && !non_resident {
            names.push(parse_file_name(resident_value(attr)?)?);
        } else if ty == ATTR_DATA && attr[9] == 0 && data.is_none() {
            data = Some(data_sizes(attr, non_resident)?);
        } else if ty == ATTR_REPARSE_POINT && !non_resident {
            let tag = u32_at(resident_value(attr)?, 0)?;
            is_reparse |= tag == TAG_SYMLINK || tag == TAG_MOUNT_POINT;
        }
        off += len;
    }

    let chosen = names
        .iter()
        .find(|n| n.namespace != NS_DOS)
        .or(names.first())?;
    let is_dir = flags & FLAG_DIRECTORY != 0;
    // shortcut: $DATA living only in an $ATTRIBUTE_LIST extension record is not followed, so such files report 0 size, upgrade by following attribute-list extension records
    let (allocated, logical) = match (is_dir, data) {
        (false, Some(sizes)) => sizes,
        _ => (0, 0),
    };

    Some(MftRecord {
        index,
        parent: chosen.parent,
        name: chosen.name.clone(),
        is_dir,
        allocated,
        logical,
        is_reparse,
    })
}

/// Undoes the update-sequence protection: checks each sector's last two bytes against the
/// sequence number, then restores the original bytes from the array. Checks all before writing.
fn apply_fixups(buf: &mut [u8]) -> Option<()> {
    let usa_off = u16_at(buf, 4)? as usize;
    let usa_count = u16_at(buf, 6)? as usize;
    if usa_count < 2 {
        return None;
    }
    let seq = u16_at(buf, usa_off)?;
    let mut originals = Vec::with_capacity(usa_count - 1);
    for sector in 1..usa_count {
        let end = sector * RECORD_SECTOR;
        if end > buf.len() || u16_at(buf, end - 2)? != seq {
            return None;
        }
        originals.push((end - 2, u16_at(buf, usa_off + 2 * sector)?));
    }
    for (pos, original) in originals {
        buf[pos..pos + 2].copy_from_slice(&original.to_le_bytes());
    }
    Some(())
}

/// (allocated, logical) from an unnamed $DATA attribute.
fn data_sizes(attr: &[u8], non_resident: bool) -> Option<(u64, u64)> {
    if non_resident {
        // @0x28 is the virtual size rounded up; compressed or sparse streams report the stored size at @0x40.
        let allocated = if u16_at(attr, 0x22)? != 0 && attr.len() >= 0x48 {
            u64_at(attr, 0x40)?
        } else {
            u64_at(attr, 0x28)?
        };
        Some((allocated, u64_at(attr, 0x30)?))
    } else {
        Some((0, resident_value(attr)?.len() as u64))
    }
}

fn resident_value(attr: &[u8]) -> Option<&[u8]> {
    let len = u32_at(attr, 0x10)? as usize;
    let off = u16_at(attr, 0x14)? as usize;
    attr.get(off..off.checked_add(len)?)
}

fn parse_file_name(value: &[u8]) -> Option<FileName> {
    let parent = u64_at(value, 0)? & PARENT_REF_MASK;
    let chars = *value.get(0x40)? as usize;
    let namespace = *value.get(0x41)?;
    let units: Vec<u16> = value
        .get(0x42..0x42 + 2 * chars)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Some(FileName {
        parent,
        namespace,
        name: String::from_utf16_lossy(&units),
    })
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REC: usize = 1024;
    const PARENT: u64 = (3 << 48) | 5;
    const FLAG_IN_USE: u16 = 0x01;
    const TYPE_FILE_NAME: u32 = 0x30;
    const TYPE_DATA: u32 = 0x80;
    const TYPE_REPARSE: u32 = 0xC0;
    const NS_DOS: u8 = 2;
    const NS_WIN32: u8 = 1;

    fn put(buf: &mut [u8], off: usize, bytes: &[u8]) {
        buf[off..off + bytes.len()].copy_from_slice(bytes);
    }

    fn align8(n: usize) -> usize {
        (n + 7) & !7
    }

    fn resident_attr(ty: u32, value: &[u8]) -> Vec<u8> {
        let len = align8(0x18 + value.len());
        let mut a = vec![0u8; len];
        put(&mut a, 0, &ty.to_le_bytes());
        put(&mut a, 4, &(len as u32).to_le_bytes());
        put(&mut a, 10, &0x18u16.to_le_bytes());
        put(&mut a, 0x10, &(value.len() as u32).to_le_bytes());
        put(&mut a, 0x14, &0x18u16.to_le_bytes());
        put(&mut a, 0x18, value);
        a
    }

    fn nonresident_attr(ty: u32, attr_flags: u16, allocated: u64, real: u64) -> Vec<u8> {
        let mut a = vec![0u8; 0x50];
        put(&mut a, 0, &ty.to_le_bytes());
        put(&mut a, 4, &0x50u32.to_le_bytes());
        a[8] = 1;
        put(&mut a, 10, &0x40u16.to_le_bytes());
        put(&mut a, 12, &attr_flags.to_le_bytes());
        put(&mut a, 0x20, &0x48u16.to_le_bytes());
        put(&mut a, 0x28, &allocated.to_le_bytes());
        put(&mut a, 0x30, &real.to_le_bytes());
        put(&mut a, 0x38, &real.to_le_bytes());
        a
    }

    /// Compressed or sparse non-resident $DATA: compression unit @0x22 and on-disk size @0x40.
    fn compressed_attr(attr_flags: u16, allocated: u64, real: u64, compressed: u64) -> Vec<u8> {
        let mut a = nonresident_attr(TYPE_DATA, attr_flags, allocated, real);
        put(&mut a, 0x22, &4u16.to_le_bytes());
        put(&mut a, 0x40, &compressed.to_le_bytes());
        a
    }

    fn file_name_value(parent: u64, namespace: u8, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42 + 2 * units.len()];
        put(&mut v, 0, &parent.to_le_bytes());
        v[0x40] = units.len() as u8;
        v[0x41] = namespace;
        for (i, u) in units.iter().enumerate() {
            put(&mut v, 0x42 + 2 * i, &u.to_le_bytes());
        }
        v
    }

    /// Builds a 1024-byte record with `names` as $FILE_NAME attributes and `attrs` appended,
    /// then applies the update-sequence fixups (sequence 0x0001 at each sector end).
    fn make_record(flags: u16, names: &[(u64, u8, &str)], attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut rec = vec![0u8; REC];
        put(&mut rec, 0, b"FILE");
        put(&mut rec, 4, &0x30u16.to_le_bytes()); // update-sequence array offset
        put(&mut rec, 6, &3u16.to_le_bytes()); // sequence + two 512-byte sectors
        put(&mut rec, 0x14, &0x38u16.to_le_bytes()); // first attribute, after the USA
        put(&mut rec, 0x16, &flags.to_le_bytes());

        let mut all: Vec<Vec<u8>> = names
            .iter()
            .map(|(p, ns, n)| resident_attr(TYPE_FILE_NAME, &file_name_value(*p, *ns, n)))
            .collect();
        all.extend(attrs.iter().cloned());

        let mut off = 0x38;
        for a in &all {
            put(&mut rec, off, a);
            off += a.len();
        }
        put(&mut rec, off, &0xFFFF_FFFFu32.to_le_bytes());

        let seq = 0x0001u16.to_le_bytes();
        put(&mut rec, 0x30, &seq);
        for sector in 1..3 {
            let end = sector * 512;
            let original = [rec[end - 2], rec[end - 1]];
            put(&mut rec, 0x30 + 2 * sector, &original);
            put(&mut rec, end - 2, &seq);
        }
        rec
    }

    #[test]
    fn parses_name_parent_and_sizes() {
        let mut rec = make_record(FLAG_IN_USE, &[(PARENT, NS_WIN32, "hello.txt")], &[]);
        let r = parse_record(&mut rec, 42).expect("record should parse");
        assert_eq!(r.index, 42);
        assert_eq!(r.parent, 5);
        assert_eq!(r.name, "hello.txt");
        assert!(!r.is_dir);
    }

    #[test]
    fn prefers_win32_name_over_dos_name() {
        let mut rec = make_record(
            FLAG_IN_USE,
            &[(5, NS_DOS, "HELLO~1.TXT"), (5, NS_WIN32, "hello world.txt")],
            &[],
        );
        let r = parse_record(&mut rec, 1).expect("record should parse");
        assert_eq!(r.name, "hello world.txt");
    }

    #[test]
    fn hard_links_use_first_non_dos_name() {
        let mut rec = make_record(
            FLAG_IN_USE,
            &[(5, NS_WIN32, "first.txt"), (9, NS_WIN32, "second.txt")],
            &[],
        );
        let r = parse_record(&mut rec, 1).expect("record should parse");
        assert_eq!(r.parent, 5);
        assert_eq!(r.name, "first.txt");
    }

    #[test]
    fn non_resident_data_uses_allocated_and_real_size() {
        let data = nonresident_attr(TYPE_DATA, 0, 8192, 5000);
        let mut rec = make_record(FLAG_IN_USE, &[(5, NS_WIN32, "data.bin")], &[data]);
        let r = parse_record(&mut rec, 1).expect("record should parse");
        assert_eq!(r.allocated, 8192);
        assert_eq!(r.logical, 5000);
    }

    #[test]
    fn resident_data_uses_value_length_and_zero_allocation() {
        let data = resident_attr(TYPE_DATA, &[0u8; 300]);
        let mut rec = make_record(FLAG_IN_USE, &[(5, NS_WIN32, "small.txt")], &[data]);
        let r = parse_record(&mut rec, 1).expect("record should parse");
        assert_eq!(r.allocated, 0);
        assert_eq!(r.logical, 300);
    }

    #[test]
    fn sparse_and_compressed_use_compressed_size_on_disk() {
        for attr_flags in [0x8000u16, 0x0001] {
            let data = compressed_attr(attr_flags, 1_048_576, 1_000_000, 65_536);
            let mut rec = make_record(FLAG_IN_USE, &[(5, NS_WIN32, "sparse.bin")], &[data]);
            let r = parse_record(&mut rec, 1).expect("record should parse");
            assert_eq!(r.allocated, 65_536);
            assert_eq!(r.logical, 1_000_000);
        }
    }

    #[test]
    fn unused_record_returns_none() {
        let mut rec = make_record(0, &[(5, NS_WIN32, "gone.txt")], &[]);
        assert!(parse_record(&mut rec, 1).is_none());
    }

    #[test]
    fn bad_fixup_returns_none() {
        let mut rec = make_record(FLAG_IN_USE, &[(5, NS_WIN32, "torn.txt")], &[]);
        rec[510] ^= 0xFF;
        assert!(parse_record(&mut rec, 1).is_none());
    }

    #[test]
    fn extension_record_returns_none() {
        let mut rec = make_record(FLAG_IN_USE, &[(5, NS_WIN32, "ext.txt")], &[]);
        rec[0x20] = 5; // base file reference is non-zero
        assert!(parse_record(&mut rec, 1).is_none());
    }

    #[test]
    fn utf16_surrogate_pair_name_decodes() {
        let mut rec = make_record(FLAG_IN_USE, &[(5, NS_WIN32, "\u{10348}.bin")], &[]);
        let r = parse_record(&mut rec, 1).expect("record should parse");
        assert_eq!(r.name, "\u{10348}.bin");
    }

    #[test]
    fn reparse_flag_is_reported() {
        let symlink = resident_attr(TYPE_REPARSE, &0xA000_000Cu32.to_le_bytes());
        let mut rec = make_record(FLAG_IN_USE, &[(5, NS_WIN32, "link")], &[symlink]);
        let r = parse_record(&mut rec, 1).expect("record should parse");
        assert!(r.is_reparse);
    }

    #[test]
    fn non_link_reparse_tag_is_not_flagged() {
        let cloud = resident_attr(TYPE_REPARSE, &0x9000_001Au32.to_le_bytes());
        let mut rec = make_record(FLAG_IN_USE, &[(5, NS_WIN32, "placeholder")], &[cloud]);
        let r = parse_record(&mut rec, 1).expect("record should parse");
        assert!(!r.is_reparse);
    }
}
