use anyhow::{bail, Context};
use std::io::{Read, Seek, SeekFrom, Write};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::iso::WalkStats;

const SECTOR_SIZE: u64 = 2048;

// Descriptor tag identifiers (ECMA-167)
const TAG_PARTITION: u16 = 0x0005;
const TAG_LOGICAL_VOLUME: u16 = 0x0006;
const TAG_TERMINATING: u16 = 0x0008;
const TAG_FILE_SET: u16 = 0x0100;
const TAG_FILE_ENTRY: u16 = 0x0105;
const TAG_EXTENDED_FILE_ENTRY: u16 = 0x010A;

fn ru16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn ru32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn ru64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

fn read_dstring(b: &[u8]) -> String {
    if b.is_empty() || b[0] == 0 {
        return String::new();
    }
    match b[0] {
        8 => String::from_utf8_lossy(&b[1..])
            .trim_end_matches('\0')
            .trim()
            .to_string(),
        16 => {
            let chars: Vec<u16> = b[1..]
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&chars)
                .trim_end_matches('\0')
                .trim()
                .to_string()
        }
        _ => String::from_utf8_lossy(&b[1..]).trim().to_string(),
    }
}

fn read_timestamp(b: &[u8]) -> Option<OffsetDateTime> {
    if b.len() < 12 {
        return None;
    }
    let tz_raw = ru16(b, 0);
    let year = ru16(b, 2) as i32;
    let month = b[4];
    let day = b[5];
    let hour = b[6];
    let minute = b[7];
    let second = b[8];

    if year == 0 && month == 0 && day == 0 {
        return None;
    }

    let date = Date::from_calendar_date(
        year,
        Month::try_from(month).ok()?,
        day,
    )
    .ok()?;
    let time = Time::from_hms(hour, minute, second).ok()?;

    // Timezone: bits 0-11 = offset in minutes (signed 12-bit), bit 12 = type
    let tz_type = (tz_raw >> 12) & 0xF;
    let offset = if tz_type == 1 {
        let raw = (tz_raw & 0x0FFF) as i16;
        // Sign-extend 12-bit value
        let signed = if raw & 0x800 != 0 {
            raw | !0x0FFF
        } else {
            raw
        };
        UtcOffset::from_whole_seconds(signed as i32 * 60).unwrap_or(UtcOffset::UTC)
    } else {
        UtcOffset::UTC
    };

    Some(PrimitiveDateTime::new(date, time).assume_offset(offset))
}

pub struct UdfEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub modified: Option<OffsetDateTime>,
    /// Absolute byte offset of file data on disk
    pub data_offset: u64,
}

pub struct UdfReader<T> {
    reader: T,
    partition_start: u64,
    block_size: u64,
    pub volume_id: String,
}

impl<T: Read + Seek> UdfReader<T> {
    pub fn new(mut reader: T) -> anyhow::Result<Self> {
        // Read Anchor Volume Descriptor Pointer at sector 256
        let avdp = read_sector(&mut reader, 256)?;
        let tag_id = ru16(&avdp, 0);
        if tag_id != 0x0002 {
            bail!("No Anchor Volume Descriptor at sector 256 (tag={:#x})", tag_id);
        }
        // Extent at offset 16: length(u32) then location(u32)
        let main_vds_len = ru32(&avdp, 16);
        let main_vds_loc = ru32(&avdp, 20);

        // Parse Volume Descriptor Sequence
        let mut partition_start: Option<u32> = None;
        let mut block_size: u32 = 2048;
        let mut fsd_location: Option<u32> = None;
        let mut volume_id = String::new();

        let max_sectors = (main_vds_len + 2047) / 2048;
        for i in 0..max_sectors.min(64) {
            let sector = (main_vds_loc + i) as u64;
            let buf = read_sector(&mut reader, sector)?;
            let tag = ru16(&buf, 0);

            match tag {
                TAG_PARTITION => {
                    partition_start = Some(ru32(&buf, 188));
                }
                TAG_LOGICAL_VOLUME => {
                    block_size = ru32(&buf, 212);
                    // LogicalVolumeContentsUse is an ExtentLong at offset 248
                    // Location is at bytes 4-7 of ExtentLong (u32 LE)
                    fsd_location = Some(ru32(&buf, 252));
                    volume_id = read_dstring(&buf[84..84 + 128]);
                }
                TAG_TERMINATING => break,
                _ => {}
            }
        }

        let partition_start = partition_start
            .context("UDF: no Partition Descriptor found")?;
        let fsd_location = fsd_location
            .context("UDF: no Logical Volume Descriptor found")?;

        // Read File Set Descriptor
        let fsd_sector = partition_start as u64 + fsd_location as u64;
        let fsd_buf = read_sector(&mut reader, fsd_sector)?;
        let fsd_tag = ru16(&fsd_buf, 0);
        if fsd_tag != TAG_FILE_SET {
            bail!(
                "Expected File Set Descriptor at sector {}, got tag {:#x}",
                fsd_sector,
                fsd_tag
            );
        }

        // RootDirectoryICB at offset 400, location at 400+4=404 (u32 in long_ad)
        let _root_icb_loc = ru32(&fsd_buf, 404);

        Ok(UdfReader {
            reader,
            partition_start: partition_start as u64,
            block_size: block_size as u64,
            volume_id,
        })
    }

    pub fn walk(&mut self) -> anyhow::Result<(Vec<UdfEntry>, WalkStats)> {
        // Re-read FSD to get root ICB
        // We need the root directory File Entry location
        // First, re-read the AVDP to re-navigate
        let avdp = read_sector(&mut self.reader, 256)?;
        let main_vds_len = ru32(&avdp, 16);
        let main_vds_loc = ru32(&avdp, 20);

        let mut fsd_location: u32 = 0;
        let max_sectors = (main_vds_len + 2047) / 2048;
        for i in 0..max_sectors.min(64) {
            let buf = read_sector(&mut self.reader, (main_vds_loc + i) as u64)?;
            let tag = ru16(&buf, 0);
            if tag == TAG_LOGICAL_VOLUME {
                fsd_location = ru32(&buf, 252);
                break;
            }
            if tag == TAG_TERMINATING {
                break;
            }
        }

        let fsd_sector = self.partition_start + fsd_location as u64;
        let fsd_buf = read_sector(&mut self.reader, fsd_sector)?;
        let root_icb_loc = ru32(&fsd_buf, 404);

        let mut entries = Vec::new();
        let mut stats = WalkStats::default();

        self.walk_dir(root_icb_loc as u64, "", &mut entries, &mut stats)?;

        Ok((entries, stats))
    }

    fn walk_dir(
        &mut self,
        fe_loc: u64,
        parent_path: &str,
        entries: &mut Vec<UdfEntry>,
        stats: &mut WalkStats,
    ) -> anyhow::Result<()> {
        let fe_sector = self.partition_start + fe_loc;
        let fe_buf = read_sector(&mut self.reader, fe_sector)?;
        let fe_tag = ru16(&fe_buf, 0);

        if fe_tag != TAG_FILE_ENTRY && fe_tag != TAG_EXTENDED_FILE_ENTRY {
            return Ok(()); // Skip non-file-entry
        }

        // Parse File Entry to get allocation descriptors
        let (alloc_descs, _info_len, _mod_time) = parse_file_entry(&fe_buf, fe_tag)?;

        if alloc_descs.is_empty() {
            return Ok(());
        }

        // Read directory data from allocation descriptors
        let mut dir_data = Vec::new();
        for ad in &alloc_descs {
            let abs_sector = self.partition_start + ad.location as u64;
            let num_sectors = (ad.length as u64 + SECTOR_SIZE - 1) / SECTOR_SIZE;
            for s in 0..num_sectors {
                let buf = read_sector(&mut self.reader, abs_sector + s)?;
                dir_data.extend_from_slice(&buf);
            }
        }

        // Parse File Identifier Descriptors
        let total_len = alloc_descs[0].length as usize;
        let total_len = total_len.min(dir_data.len());
        let mut offset = 0usize;

        while offset + 38 < total_len {
            let fid = &dir_data[offset..];
            if fid.len() < 38 {
                break;
            }

            let fid_tag = ru16(fid, 0);
            if fid_tag != 0x0101 {
                break;
            }

            let characteristics = fid[18];
            let ident_len = fid[19] as usize;
            let impl_use_len = ru16(fid, 36) as usize;
            let ident_start = 38 + impl_use_len;

            // Calculate FID total length with padding to 4-byte boundary
            let fid_len = 38 + impl_use_len + ident_len;
            let fid_len_padded = (fid_len + 3) & !3;

            let is_parent = characteristics & 0x08 != 0;
            let is_deleted = characteristics & 0x20 != 0;
            let is_dir = characteristics & 0x02 != 0;

            if !is_parent && !is_deleted && ident_start + ident_len <= fid.len() {
                let name = read_dstring(&fid[ident_start..ident_start + ident_len]);
                if !name.is_empty() {
                    let icb_loc = ru32(fid, 24); // location within long_ad at offset 20

                    let path = if parent_path.is_empty() {
                        format!("/{}", name)
                    } else {
                        format!("{}/{}", parent_path, name)
                    };

                    if is_dir {
                        stats.dirs += 1;
                        self.walk_dir(icb_loc as u64, &path, entries, stats)?;
                    } else {
                        // Read file's File Entry for size, timestamp, data location
                        let file_fe_sector = self.partition_start + icb_loc as u64;
                        if let Ok(file_fe_buf) = read_sector(&mut self.reader, file_fe_sector) {
                            let file_fe_tag = ru16(&file_fe_buf, 0);
                            if file_fe_tag == TAG_FILE_ENTRY
                                || file_fe_tag == TAG_EXTENDED_FILE_ENTRY
                            {
                                if let Ok((file_ads, info_len, mod_time)) =
                                    parse_file_entry(&file_fe_buf, file_fe_tag)
                                {
                                    let data_offset = if !file_ads.is_empty() {
                                        (self.partition_start + file_ads[0].location as u64)
                                            * SECTOR_SIZE
                                    } else {
                                        0
                                    };

                                    stats.files += 1;
                                    stats.total_size += info_len;

                                    entries.push(UdfEntry {
                                        path,
                                        size: info_len,
                                        is_dir: false,
                                        modified: mod_time,
                                        data_offset,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            offset += fid_len_padded;
        }

        Ok(())
    }

    pub fn read_file_to<W: Write>(
        &mut self,
        entry: &UdfEntry,
        writer: &mut W,
    ) -> anyhow::Result<u64> {
        if entry.data_offset == 0 || entry.size == 0 {
            return Ok(0);
        }

        self.reader.seek(SeekFrom::Start(entry.data_offset))?;

        let mut remaining = entry.size;
        let mut buf = [0u8; 64 * 1024];
        let mut written = 0u64;

        while remaining > 0 {
            let to_read = (remaining as usize).min(buf.len());
            let n = self.reader.read(&mut buf[..to_read])?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
            remaining -= n as u64;
            written += n as u64;
        }

        Ok(written)
    }
}

struct AllocDesc {
    length: u32,
    location: u32,
}

fn parse_file_entry(
    buf: &[u8],
    tag: u16,
) -> anyhow::Result<(Vec<AllocDesc>, u64, Option<OffsetDateTime>)> {
    // File Entry (tag 0x0105) and Extended File Entry (tag 0x010A) have
    // different layouts. The key fields we need:
    let (info_len, mod_time, len_ea, len_ad, ad_base) = if tag == TAG_FILE_ENTRY {
        // Standard File Entry (ECMA-167 §14.9)
        let info_len = ru64(buf, 56);
        let mod_time = read_timestamp(&buf[84..]);
        let len_ea = ru32(buf, 168);
        let len_ad = ru32(buf, 172);
        let ad_base = 176 + len_ea as usize;
        (info_len, mod_time, len_ea, len_ad, ad_base)
    } else {
        // Extended File Entry (ECMA-167 §14.17)
        // Offsets shift because of additional fields
        let info_len = ru64(buf, 56);
        let mod_time = read_timestamp(&buf[84..]);
        let len_ea = ru32(buf, 208);
        let len_ad = ru32(buf, 212);
        let ad_base = 216 + len_ea as usize;
        (info_len, mod_time, len_ea, len_ad, ad_base)
    };

    let mut alloc_descs = Vec::new();
    let ad_end = ad_base + len_ad as usize;
    let mut pos = ad_base;

    // Parse short allocation descriptors (8 bytes each)
    while pos + 8 <= ad_end && pos + 8 <= buf.len() {
        let length = ru32(buf, pos) & 0x3FFFFFFF; // Mask out extent type bits
        let location = ru32(buf, pos + 4);
        if length == 0 {
            break;
        }
        alloc_descs.push(AllocDesc { length, location });
        pos += 8;
    }

    Ok((alloc_descs, info_len, mod_time))
}

fn read_sector<T: Read + Seek>(reader: &mut T, sector: u64) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; SECTOR_SIZE as usize];
    reader.seek(SeekFrom::Start(sector * SECTOR_SIZE))?;
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

/// Check if an ISO file contains a UDF filesystem
pub fn is_udf<T: Read + Seek>(reader: &mut T) -> bool {
    // Check for BEA01/NSR02/NSR03 in sectors 16-31
    for sector in 16u64..32 {
        let mut buf = [0u8; 2048];
        if reader.seek(SeekFrom::Start(sector * 2048)).is_err() {
            return false;
        }
        if reader.read_exact(&mut buf).is_err() {
            return false;
        }
        if &buf[1..6] == b"NSR02" || &buf[1..6] == b"NSR03" {
            return true;
        }
    }
    false
}
