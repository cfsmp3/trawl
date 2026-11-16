//! Keeps a copy of the resources in `$XDG_RUNTIME_DIR/trawl`, so a trawld
//! that is restarted on the same session bus gets back the resources the
//! previous instance held, like resources kept by the X server outlive
//! their clients.
//!
//! The snapshot is rewritten as a whole after every change: it is written
//! to a temporary file that is renamed over the previous snapshot, so it
//! always holds either the old or the new resources. It carries the id of
//! the session bus it belongs to and a checksum, so a snapshot of another
//! session or a damaged one is never restored.

use std::{
    collections::HashMap,
    fs::{self, DirBuilder, OpenOptions, Permissions},
    io::{self, ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const DIR_NAME: &str = "trawl";
const FILE_NAME: &str = "resources";
const TMP_NAME: &str = "resources.tmp";
const MAGIC: &[u8] = b"trawld snapshot 1\n";

extern "C" {
    fn geteuid() -> u32;
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail
    unsafe { geteuid() }
}

/// What a snapshot held when it was read
#[derive(Debug, PartialEq, Eq)]
pub enum Restored {
    /// No snapshot has been saved yet
    Nothing,
    /// The snapshot belongs to another session bus and is discarded
    OtherBus,
    /// The resources saved on this session bus
    Resources(HashMap<String, String>),
}

/// The saved copy of the resources for one session bus
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    dir: PathBuf,
    bus_id: String,
}

/// The directory trawld keeps its snapshot in: `$XDG_RUNTIME_DIR/trawl`.
/// Fails when `XDG_RUNTIME_DIR` is not set; trawld then keeps its resources
/// in memory only.
pub fn runtime_dir() -> Result<PathBuf, String> {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => Ok(Path::new(&dir).join(DIR_NAME)),
        _ => Err(String::from(
            "XDG_RUNTIME_DIR is not set, resources will not survive a restart",
        )),
    }
}

impl Snapshot {
    /// The snapshot kept in `dir` for the bus with `bus_id`. `dir` doesn't
    /// have to exist yet: it is created by the first save. Fails when `dir`
    /// is not safe to use: a symbolic link, not a directory, owned by
    /// another user, or writable by the group or others.
    pub fn in_dir(dir: PathBuf, bus_id: &str) -> Result<Snapshot, String> {
        let snapshot = Snapshot {
            dir,
            bus_id: bus_id.to_owned(),
        };
        snapshot.check_dir()?;
        Ok(snapshot)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns whether the directory exists; fails unless it is a real
    /// directory owned by this user that only this user can write to.
    fn check_dir(&self) -> Result<bool, String> {
        let dir = self.dir.display();
        let meta = match fs::symlink_metadata(&self.dir) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(format!("cannot use {dir}: {e}")),
        };
        if meta.file_type().is_symlink() {
            return Err(format!("not using {dir}: it is a symbolic link"));
        }
        if !meta.is_dir() {
            return Err(format!("not using {dir}: it is not a directory"));
        }
        if meta.uid() != effective_uid() {
            return Err(format!(
                "not using {dir}: it is owned by uid {}",
                meta.uid()
            ));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!(
                "not using {dir}: it is writable by others (mode {:o})",
                meta.mode() & 0o7777
            ));
        }
        Ok(true)
    }

    /// Reads the saved resources
    pub fn restore(&self) -> Result<Restored, String> {
        if !self.check_dir()? {
            return Ok(Restored::Nothing);
        }
        let path = self.dir.join(FILE_NAME);
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Restored::Nothing),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        if !meta.is_file() {
            return Err(format!(
                "not reading {}: it is not a regular file",
                path.display()
            ));
        }
        let data = fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let (bus_id, resources) = decode(&data)
            .ok_or_else(|| format!("{} is damaged and was not restored", path.display()))?;
        if bus_id != self.bus_id {
            return Ok(Restored::OtherBus);
        }
        Ok(Restored::Resources(resources))
    }

    /// Replaces the saved resources with `resources`
    pub fn save(&self, resources: &HashMap<String, String>) -> Result<(), String> {
        if !self.check_dir()? {
            DirBuilder::new()
                .mode(0o700)
                .create(&self.dir)
                .and_then(|_| fs::set_permissions(&self.dir, Permissions::from_mode(0o700)))
                .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
            self.check_dir()?;
        }
        let tmp = self.dir.join(TMP_NAME);
        let data = encode(&self.bus_id, resources);
        self.write_tmp(&tmp, &data)
            .and_then(|_| fs::rename(&tmp, self.dir.join(FILE_NAME)))
            .map_err(|e| {
                let _ = fs::remove_file(&tmp);
                format!("cannot save resources in {}: {e}", self.dir.display())
            })
    }

    fn write_tmp(&self, tmp: &Path, data: &[u8]) -> io::Result<()> {
        // A leftover from a save that was interrupted
        match fs::remove_file(tmp) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => (),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(tmp)?;
        file.set_permissions(Permissions::from_mode(0o600))?;
        file.write_all(data)
    }
}

const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const CRC32_TABLE: [u32; 256] = crc32_table();

fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |crc, &byte| {
        CRC32_TABLE[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8)
    })
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Layout: magic, bus id, number of resources, then every name and value;
/// each string is its length (u32, little endian) followed by its bytes.
/// A CRC-32 of everything before it ends the snapshot.
fn encode(bus_id: &str, resources: &HashMap<String, String>) -> Vec<u8> {
    let size: usize = resources.iter().map(|(k, v)| k.len() + v.len() + 8).sum();
    let mut out = Vec::with_capacity(MAGIC.len() + bus_id.len() + size + 16);
    out.extend_from_slice(MAGIC);
    put_str(&mut out, bus_id);
    out.extend_from_slice(&(resources.len() as u64).to_le_bytes());
    for (key, val) in resources {
        put_str(&mut out, key);
        put_str(&mut out, val);
    }
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.data.len() {
            return None;
        }
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }
}

/// The bus id and resources of a snapshot, or None when it is damaged
fn decode(data: &[u8]) -> Option<(String, HashMap<String, String>)> {
    let body_len = data.len().checked_sub(4)?;
    let (body, crc) = data.split_at(body_len);
    if crc32(body) != u32::from_le_bytes(crc.try_into().ok()?) {
        return None;
    }
    let mut reader = Reader { data: body };
    if reader.take(MAGIC.len())? != MAGIC {
        return None;
    }
    let bus_id = reader.string()?;
    let count = reader.u64()?;
    let mut resources = HashMap::new();
    for _ in 0..count {
        let key = reader.string()?;
        let val = reader.string()?;
        if resources.insert(key, val).is_some() {
            return None;
        }
    }
    if !reader.data.is_empty() {
        return None;
    }
    Some((bus_id, resources))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn sample() -> HashMap<String, String> {
        [
            ("plain", "value"),
            ("", "empty name"),
            ("empty", ""),
            (
                "multi\nline",
                "a\tb: c\n!d #e \\f \\U000000e9 café \u{FFFD}",
            ),
        ]
        .iter()
        .map(|&(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn tmp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("trawld-snapshot-{}", Uuid::new_v4()));
        dir
    }

    #[test]
    fn encode_decode_roundtrip() {
        let resources = sample();
        let data = encode("bus-a", &resources);
        assert_eq!(decode(&data), Some((String::from("bus-a"), resources)));
    }

    #[test]
    fn damage_is_detected() {
        let data = encode("bus-a", &sample());
        for len in 0..data.len() {
            assert_eq!(decode(&data[..len]), None, "truncated to {len}");
        }
        for pos in 0..data.len() {
            let mut damaged = data.clone();
            damaged[pos] ^= 0x01;
            assert_eq!(decode(&damaged), None, "byte {pos} flipped");
        }
    }

    #[test]
    fn save_and_restore() {
        let dir = tmp_dir();
        let snapshot = Snapshot::in_dir(dir.clone(), "bus-a").unwrap();
        assert_eq!(snapshot.restore(), Ok(Restored::Nothing));
        snapshot.save(&sample()).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        let file = fs::metadata(dir.join(FILE_NAME)).unwrap();
        assert_eq!(file.mode() & 0o777, 0o600);
        assert_eq!(snapshot.restore(), Ok(Restored::Resources(sample())));
        let other = Snapshot::in_dir(dir.clone(), "bus-b").unwrap();
        assert_eq!(other.restore(), Ok(Restored::OtherBus));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unsafe_dir_is_refused() {
        let dir = tmp_dir();
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, Permissions::from_mode(0o770)).unwrap();
        assert!(Snapshot::in_dir(dir.clone(), "bus-a").is_err());
        fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();
        let link = tmp_dir();
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(Snapshot::in_dir(link.clone(), "bus-a").is_err());
        fs::remove_file(&link).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
