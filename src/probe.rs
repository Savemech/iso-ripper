use anyhow::Context;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub struct ProbeResult {
    pub file_size: u64,
    pub has_iso9660: bool,
    pub has_joliet: bool,
    pub has_rock_ridge: bool,
    pub has_udf: bool,
    pub has_el_torito: bool,
}

impl ProbeResult {
    pub fn format_label(&self) -> String {
        if !self.has_iso9660 && !self.has_udf {
            return "Unknown".into();
        }
        let mut parts = Vec::new();
        if self.has_iso9660 {
            parts.push("ISO9660");
        }
        if self.has_joliet {
            parts.push("Joliet");
        }
        if self.has_rock_ridge {
            parts.push("RockRidge");
        }
        if self.has_udf {
            parts.push("UDF");
        }
        if self.has_el_torito {
            parts.push("ElTorito");
        }
        parts.join("+")
    }

    pub fn is_supported(&self) -> bool {
        self.has_iso9660
    }
}

pub fn probe_iso(path: &Path) -> anyhow::Result<ProbeResult> {
    let mut file = File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let file_size = file.metadata()?.len();

    // Need at least sector 16 + one sector to check descriptors
    if file_size < 0x8000 + 2048 {
        return Ok(ProbeResult {
            file_size,
            has_iso9660: false,
            has_joliet: false,
            has_rock_ridge: false,
            has_udf: false,
            has_el_torito: false,
        });
    }

    let mut buf = [0u8; 2048];

    // Check sector 16 for ISO9660 Primary Volume Descriptor (type 1, "CD001")
    file.seek(SeekFrom::Start(0x8000))?;
    file.read_exact(&mut buf)?;
    let has_iso9660 = buf[0] == 1 && &buf[1..6] == b"CD001";

    // Scan sectors 16-31 for Joliet, El Torito, UDF
    let mut has_joliet = false;
    let mut has_udf = false;
    let mut has_el_torito = false;

    for sector in 16u64..32 {
        let offset = sector * 2048;
        if offset + 2048 > file_size {
            break;
        }
        file.seek(SeekFrom::Start(offset))?;
        if file.read_exact(&mut buf).is_err() {
            break;
        }

        // Joliet: supplementary volume descriptor (type 2) with UCS-2 escape sequences
        if buf[0] == 2 && &buf[1..6] == b"CD001" {
            let esc = &buf[88..91];
            if esc == b"\x25\x2F\x40"
                || esc == b"\x25\x2F\x43"
                || esc == b"\x25\x2F\x45"
            {
                has_joliet = true;
            }
        }

        // El Torito: boot record volume descriptor (type 0)
        if buf[0] == 0
            && &buf[1..6] == b"CD001"
            && buf[7..].starts_with(b"EL TORITO SPECIFICATION")
        {
            has_el_torito = true;
        }

        // UDF: Beginning Extended Area Descriptor or NSR descriptors
        if &buf[1..6] == b"BEA01"
            || &buf[1..6] == b"NSR02"
            || &buf[1..6] == b"NSR03"
        {
            has_udf = true;
        }

        // Volume Descriptor Set Terminator
        if buf[0] == 255 && &buf[1..6] == b"CD001" {
            break;
        }
    }

    // Rock Ridge: open with cdfs and check
    let mut has_rock_ridge = false;
    if has_iso9660 {
        file.seek(SeekFrom::Start(0))?;
        if let Ok(iso) = cdfs::ISO9660::new(file) {
            has_rock_ridge = iso.is_rr();
        }
    }

    Ok(ProbeResult {
        file_size,
        has_iso9660,
        has_joliet,
        has_rock_ridge,
        has_udf,
        has_el_torito,
    })
}
