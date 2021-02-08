// Ported from https://github.com/randall77/makefat/blob/master/makefat.go
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use goblin::{
    mach::{
        cputype::{
            get_arch_from_flag, get_arch_name_from_types, CpuSubType, CpuType, CPU_TYPE_ARM,
            CPU_TYPE_ARM64, CPU_TYPE_ARM64_32, CPU_TYPE_HPPA, CPU_TYPE_I386, CPU_TYPE_I860,
            CPU_TYPE_MC680X0, CPU_TYPE_MC88000, CPU_TYPE_POWERPC, CPU_TYPE_POWERPC64,
            CPU_TYPE_SPARC, CPU_TYPE_X86_64,
        },
        fat::FAT_MAGIC,
        header::Header,
        Mach,
    },
    Object,
};

use crate::error::Error;
use std::cmp::Ordering;

const FAT_MAGIC_64: u32 = FAT_MAGIC + 1;

#[derive(Debug)]
struct ThinArch {
    data: Vec<u8>,
    header: Header,
    align: i64,
}

/// Mach-O fat binary writer
#[derive(Debug)]
pub struct FatWriter {
    arches: Vec<ThinArch>,
    max_align: i64,
}

impl FatWriter {
    /// Create a new Mach-O fat binary writer
    pub fn new() -> Self {
        Self {
            arches: Vec::new(),
            max_align: 0,
        }
    }

    /// Add a new thin Mach-O binary
    pub fn add<T: Into<Vec<u8>>>(&mut self, bytes: T) -> Result<(), Error> {
        let bytes = bytes.into();
        match Object::parse(&bytes)? {
            Object::Mach(mach) => match mach {
                Mach::Fat(fat) => {
                    for arch in fat.arches()? {
                        let buffer = arch.slice(&bytes);
                        self.add(buffer.to_vec())?;
                    }
                }
                Mach::Binary(obj) => {
                    let header = obj.header;
                    let cpu_type = header.cputype;
                    let cpu_subtype = header.cpusubtype;
                    // Check if this architecture already exists
                    if self
                        .arches
                        .iter()
                        .find(|arch| {
                            arch.header.cputype == cpu_type && arch.header.cpusubtype == cpu_subtype
                        })
                        .is_some()
                    {
                        let arch =
                            get_arch_name_from_types(cpu_type, cpu_subtype).unwrap_or("unknown");
                        return Err(Error::DuplicatedArch(arch.to_string()));
                    }
                    let align = get_align_from_cpu_types(cpu_type, cpu_subtype);
                    if align > self.max_align {
                        self.max_align = align;
                    }
                    let thin = ThinArch {
                        data: bytes,
                        header: header,
                        align,
                    };
                    self.arches.push(thin);
                }
            },
            _ => return Err(Error::InvalidMachO("input is not a macho file".to_string())),
        }
        // Sort the files by alignment to save space in ouput
        self.arches.sort_by(|a, b| {
            if a.header.cputype == b.header.cputype {
                // if cpu types match, sort by cpu subtype
                return a.header.cpusubtype.cmp(&b.header.cpusubtype);
            }
            // force arm64-family to follow after all other slices
            if a.header.cputype == CPU_TYPE_ARM64 {
                return Ordering::Greater;
            }
            if b.header.cputype == CPU_TYPE_ARM64 {
                return Ordering::Less;
            }
            a.align.cmp(&b.align)
        });
        Ok(())
    }

    /// Remove an architecture
    pub fn remove(&mut self, arch: &str) -> Option<Vec<u8>> {
        if let Some((cpu_type, cpu_subtype)) = get_arch_from_flag(arch) {
            if let Some(index) = self.arches.iter().position(|arch| {
                arch.header.cputype == cpu_type && arch.header.cpusubtype == cpu_subtype
            }) {
                return Some(self.arches.remove(index).data);
            }
        }
        None
    }

    /// Check whether a certain architecture exists in this fat binary
    pub fn exists(&self, arch: &str) -> bool {
        if let Some((cpu_type, cpu_subtype)) = get_arch_from_flag(arch) {
            return self
                .arches
                .iter()
                .find(|arch| {
                    arch.header.cputype == cpu_type && arch.header.cpusubtype == cpu_subtype
                })
                .is_some();
        }
        false
    }

    /// Write Mach-O fat binary into the writer
    pub fn write_to<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        if self.arches.is_empty() {
            return Ok(());
        }
        let align = self.max_align;
        let mut total_offset = align;
        let mut arch_offsets = Vec::with_capacity(self.arches.len());
        for arch in &self.arches {
            arch_offsets.push(total_offset);
            total_offset += arch.data.len() as i64;
            total_offset = (total_offset + align - 1) / align * align;
        }
        // Check whether we're doing fat32 or fat64
        let is_fat64 = if total_offset >= 1i64 << 32
            || self.arches.last().unwrap().data.len() as i64 >= 1i64 << 32
        {
            true
        } else {
            false
        };
        let mut hdr = Vec::with_capacity(12);
        // Build a fat_header
        if is_fat64 {
            hdr.push(FAT_MAGIC_64);
        } else {
            hdr.push(FAT_MAGIC);
        }
        hdr.push(self.arches.len() as u32);
        // Compute the max alignment bits
        let align_bits = (align as f32).log2() as u32;
        // Build a fat_arch for each arch
        for (arch, arch_offset) in self.arches.iter().zip(arch_offsets.iter()) {
            hdr.push(arch.header.cputype);
            hdr.push(arch.header.cpusubtype);
            if is_fat64 {
                // Big Endian
                hdr.push((arch_offset >> 32) as u32);
            }
            hdr.push(*arch_offset as u32);
            if is_fat64 {
                hdr.push((arch.data.len() >> 32) as u32);
            }
            hdr.push(arch.data.len() as u32);
            hdr.push(align_bits);
            if is_fat64 {
                // Reserved
                hdr.push(0);
            }
        }
        // Write header
        // Note that the fat binary header is big-endian, regardless of the
        // endianness of the contained files.
        for i in &hdr {
            writer.write_all(&i.to_be_bytes())?;
        }
        let mut offset = 4 * hdr.len() as i64;
        // Write each arch
        for (arch, arch_offset) in self.arches.iter().zip(arch_offsets) {
            if offset < arch_offset {
                writer.write_all(&vec![0; (arch_offset - offset) as usize])?;
                offset = arch_offset;
            }
            writer.write_all(&arch.data)?;
            offset += arch.data.len() as i64;
        }
        Ok(())
    }

    /// Write Mach-O fat binary to a file
    pub fn write_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), Error> {
        let file = File::create(path)?;
        #[cfg(unix)]
        {
            let mut perm = file.metadata()?.permissions();
            perm.set_mode(0o755);
            file.set_permissions(perm)?;
        }
        let mut writer = BufWriter::new(file);
        self.write_to(&mut writer)?;
        Ok(())
    }
}

fn get_align_from_cpu_types(cpu_type: CpuType, cpu_subtype: CpuSubType) -> i64 {
    if let Some(arch_name) = get_arch_name_from_types(cpu_type, cpu_subtype) {
        if let Some((cpu_type, _)) = get_arch_from_flag(arch_name) {
            match cpu_type {
                // embedded
                CPU_TYPE_ARM | CPU_TYPE_ARM64 | CPU_TYPE_ARM64_32 => return 0x4000,
                // desktop
                CPU_TYPE_X86_64 | CPU_TYPE_I386 | CPU_TYPE_POWERPC | CPU_TYPE_POWERPC64 => {
                    return 0x1000
                }
                CPU_TYPE_MC680X0 | CPU_TYPE_MC88000 | CPU_TYPE_SPARC | CPU_TYPE_I860
                | CPU_TYPE_HPPA => return 0x2000,
                _ => {}
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::FatWriter;
    use crate::read::FatReader;

    #[test]
    fn test_fat_writer_exe() {
        let mut fat = FatWriter::new();
        let f1 = fs::read("tests/fixtures/thin_x86_64").unwrap();
        let f2 = fs::read("tests/fixtures/thin_arm64").unwrap();
        fat.add(f1).unwrap();
        fat.add(f2).unwrap();
        let mut out = Vec::new();
        fat.write_to(&mut out).unwrap();

        let reader = FatReader::new(&out);
        assert!(reader.is_ok());

        fat.write_to_file("tests/output/fat").unwrap();
    }

    #[test]
    fn test_fat_writer_add_duplicated_arch() {
        let mut fat = FatWriter::new();
        let f1 = fs::read("tests/fixtures/thin_x86_64").unwrap();
        fat.add(f1.clone()).unwrap();
        assert!(fat.add(f1).is_err());
    }

    #[test]
    fn test_fat_writer_add_fat() {
        let mut fat = FatWriter::new();
        let f1 = fs::read("tests/fixtures/simplefat").unwrap();
        fat.add(f1).unwrap();
        assert!(fat.exists("x86_64"));
        assert!(fat.exists("arm64"));
    }

    #[test]
    fn test_fat_writer_remove() {
        let mut fat = FatWriter::new();
        let f1 = fs::read("tests/fixtures/thin_x86_64").unwrap();
        let f2 = fs::read("tests/fixtures/thin_arm64").unwrap();
        fat.add(f1).unwrap();
        fat.add(f2).unwrap();
        let arm64 = fat.remove("arm64");
        assert!(arm64.is_some());
        assert!(fat.exists("x86_64"));
        assert!(!fat.exists("arm64"));
    }
}
