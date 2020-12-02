use crate::app_memory::AppMemoryList;
use crate::dso_debug;
use crate::linux_ptrace_dumper::LinuxPtraceDumper;
use crate::maps_reader::{MappingInfo, MappingList};
use crate::minidump_format::*;
use crate::sections::*;
use crate::thread_info::Pid;
use crate::Result;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};

pub type DumpBuf = Cursor<Vec<u8>>;

#[derive(Debug)]
pub struct DirSection<'a, W>
where
    W: Write + Seek,
{
    curr_idx: usize,
    section: MemoryArrayWriter<MDRawDirectory>,
    /// If we have to append to some file, we have to know where we currently are
    destination_start_offset: u64,
    destination: &'a mut W,
    last_position_written_to_file: u64,
}

impl<'a, W> DirSection<'a, W>
where
    W: Write + Seek,
{
    fn new(buffer: &mut DumpBuf, index_length: u32, destination: &'a mut W) -> Result<Self> {
        let dir_section =
            MemoryArrayWriter::<MDRawDirectory>::alloc_array(buffer, index_length as usize)?;
        Ok(DirSection {
            curr_idx: 0,
            section: dir_section,
            destination_start_offset: destination.seek(SeekFrom::Current(0))?,
            destination,
            last_position_written_to_file: 0,
        })
    }

    fn position(&self) -> u32 {
        self.section.position
    }

    fn dump_dir_entry(&mut self, buffer: &mut DumpBuf, dirent: MDRawDirectory) -> Result<()> {
        self.section.set_value_at(buffer, dirent, self.curr_idx)?;

        // Now write it to file

        // First get all the positions
        let curr_file_pos = self.destination.seek(SeekFrom::Current(0))?;
        let idx_pos = self.section.location_of_index(self.curr_idx);
        self.curr_idx += 1;

        self.destination.seek(std::io::SeekFrom::Start(
            self.destination_start_offset + idx_pos.rva as u64,
        ))?;
        let start = idx_pos.rva as usize;
        let end = (idx_pos.rva + idx_pos.data_size) as usize;
        self.destination.write_all(&buffer.get_ref()[start..end])?;

        // Reset file-position
        self.destination
            .seek(std::io::SeekFrom::Start(curr_file_pos))?;

        Ok(())
    }

    /// Writes 2 things to file:
    /// 1. The given dirent into the dir section in the header (if any is given)
    /// 2. Everything in the in-memory buffer that was added since the last call to this function
    fn write_to_file(
        &mut self,
        buffer: &mut DumpBuf,
        dirent: Option<MDRawDirectory>,
    ) -> Result<()> {
        if let Some(dirent) = dirent {
            self.dump_dir_entry(buffer, dirent)?;
        }

        let start_pos = self.last_position_written_to_file as usize;
        self.destination.write_all(&buffer.get_ref()[start_pos..])?;
        self.last_position_written_to_file = buffer.position();
        Ok(())
    }
}

#[derive(Debug)]
pub struct MinidumpWriter {
    pub process_id: Pid,
    pub blamed_thread: Pid,
    pub minidump_size_limit: Option<u64>,
    pub skip_stacks_if_mapping_unreferenced: bool,
    pub principal_mapping_address: Option<usize>,
    pub user_mapping_list: MappingList,
    pub app_memory: AppMemoryList,
    pub memory_blocks: Vec<MDMemoryDescriptor>,
    pub principal_mapping: Option<MappingInfo>,
    pub sanitize_stack: bool,
}

// This doesn't work yet:
// https://github.com/rust-lang/rust/issues/43408
// fn write<T: Sized, P: AsRef<Path>>(path: P, value: T) -> Result<()> {
//     let mut file = std::fs::File::open(path)?;
//     let bytes: [u8; size_of::<T>()] = unsafe { transmute(value) };
//     file.write_all(&bytes)?;
//     Ok(())
// }

impl MinidumpWriter {
    pub fn new(process: Pid, blamed_thread: Pid) -> Self {
        MinidumpWriter {
            process_id: process,
            blamed_thread,
            minidump_size_limit: None,
            skip_stacks_if_mapping_unreferenced: false,
            principal_mapping_address: None,
            user_mapping_list: MappingList::new(),
            app_memory: AppMemoryList::new(),
            memory_blocks: Vec::new(),
            principal_mapping: None,
            sanitize_stack: false,
        }
    }

    pub fn set_minidump_size_limit(&mut self, limit: u64) -> &mut Self {
        self.minidump_size_limit = Some(limit);
        self
    }

    pub fn set_user_mapping_list(&mut self, user_mapping_list: MappingList) -> &mut Self {
        self.user_mapping_list = user_mapping_list;
        self
    }

    pub fn set_principal_mapping_address(&mut self, principal_mapping_address: usize) -> &mut Self {
        self.principal_mapping_address = Some(principal_mapping_address);
        self
    }

    pub fn set_app_memory(&mut self, app_memory: AppMemoryList) -> &mut Self {
        self.app_memory = app_memory;
        self
    }

    pub fn skip_stacks_if_mapping_unreferenced(&mut self) -> &mut Self {
        self.skip_stacks_if_mapping_unreferenced = true; // Off by default
        self
    }

    pub fn sanitize_stack(&mut self) -> &mut Self {
        self.sanitize_stack = true; // Off by default
        self
    }

    /// Generates a minidump and writes to the destination provided. Returns the in-memory
    /// version of the minidump as well.
    pub fn dump(&mut self, destination: &mut (impl Write + Seek)) -> Result<Vec<u8>> {
        let mut dumper = LinuxPtraceDumper::new(self.process_id)?;
        dumper.suspend_threads()?;
        // TODO: Doesn't exist yet
        //self.dumper.late_init()?;

        if self.skip_stacks_if_mapping_unreferenced {
            if let Some(address) = self.principal_mapping_address {
                self.principal_mapping = dumper.find_mapping_no_bias(address).cloned();
            }
            // This is currently always false, as we don't yet support ucontext.
            //if (!CrashingThreadReferencesPrincipalMapping())
            if true {
                return Err("!CrashingThreadReferencesPrincipalMapping".into());
            }
        }

        let mut buffer = Cursor::new(Vec::new());
        self.generate_dump(&mut buffer, &mut dumper, destination)?;

        // dumper would resume threads in drop() automatically,
        // but in case there is an error, we want to catch it
        dumper.resume_threads()?;

        Ok(buffer.into_inner())
    }

    fn generate_dump(
        &mut self,
        buffer: &mut DumpBuf,
        dumper: &mut LinuxPtraceDumper,
        destination: &mut (impl Write + Seek),
    ) -> Result<()> {
        // A minidump file contains a number of tagged streams. This is the number
        // of stream which we write.
        let num_writers = 13u32;

        let mut header_section = MemoryWriter::<MDRawHeader>::alloc(buffer)?;

        let mut dir_section = DirSection::new(buffer, num_writers, destination)?;

        let header = MDRawHeader {
            signature: MD_HEADER_SIGNATURE,
            version: MD_HEADER_VERSION,
            stream_count: num_writers,
            //   header.get()->stream_directory_rva = dir.position();
            stream_directory_rva: dir_section.position(),
            checksum: 0, /* Can be 0.  In fact, that's all that's
                          * been found in minidump files. */
            time_date_stamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as u32, // TODO: This is not Y2038 safe, but thats how its currently defined as
            flags: 0,
        };
        header_section.set_value(buffer, header)?;

        // Ensure the header gets flushed. If we crash somewhere below,
        // we should have a mostly-intact dump
        dir_section.write_to_file(buffer, None)?;

        let dirent = thread_list_stream::write(self, buffer, &dumper)?;
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        let dirent = mappings::write(self, buffer, dumper)?;
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        let _ = app_memory::write(self, buffer)?;
        // Write section to file
        dir_section.write_to_file(buffer, None)?;

        let dirent = memory_list_stream::write(self, buffer)?;
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        // Currently unused
        let dirent = exception_stream::write(self, buffer)?;
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        let dirent = systeminfo_stream::write(buffer)?;
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        let dirent = match self.write_file(buffer, "/proc/cpuinfo") {
            Ok(location) => MDRawDirectory {
                stream_type: MDStreamType::LinuxCpuInfo as u32,
                location,
            },
            Err(_) => Default::default(),
        };
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        let dirent = match self.write_file(buffer, &format!("/proc/{}/status", self.blamed_thread))
        {
            Ok(location) => MDRawDirectory {
                stream_type: MDStreamType::LinuxProcStatus as u32,
                location,
            },
            Err(_) => Default::default(),
        };
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        let dirent = match self
            .write_file(buffer, "/etc/lsb-release")
            .or_else(|_| self.write_file(buffer, "/etc/os-release"))
        {
            Ok(location) => MDRawDirectory {
                stream_type: MDStreamType::LinuxLsbRelease as u32,
                location,
            },
            Err(_) => Default::default(),
        };
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        let dirent = match self.write_file(buffer, &format!("/proc/{}/cmdline", self.blamed_thread))
        {
            Ok(location) => MDRawDirectory {
                stream_type: MDStreamType::LinuxCmdLine as u32,
                location,
            },
            Err(_) => Default::default(),
        };
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        let dirent = match self.write_file(buffer, &format!("/proc/{}/environ", self.blamed_thread))
        {
            Ok(location) => MDRawDirectory {
                stream_type: MDStreamType::LinuxEnviron as u32,
                location,
            },
            Err(_) => Default::default(),
        };
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        let dirent = match self.write_file(buffer, &format!("/proc/{}/auxv", self.blamed_thread)) {
            Ok(location) => MDRawDirectory {
                stream_type: MDStreamType::LinuxAuxv as u32,
                location,
            },
            Err(_) => Default::default(),
        };
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        let dirent = match self.write_file(buffer, &format!("/proc/{}/maps", self.blamed_thread)) {
            Ok(location) => MDRawDirectory {
                stream_type: MDStreamType::LinuxMaps as u32,
                location,
            },
            Err(_) => Default::default(),
        };
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        let dirent = dso_debug::write_dso_debug_stream(buffer, self.blamed_thread, &dumper.auxv)?;
        // Write section to file
        dir_section.write_to_file(buffer, Some(dirent))?;

        // If you add more directory entries, don't forget to update kNumWriters,
        // above.
        Ok(())
    }

    fn write_file(&self, buffer: &mut DumpBuf, filename: &str) -> Result<MDLocationDescriptor> {
        // TODO: Is this buffer-limitation really needed? Or could we read&write all?
        // We can't stat the files because several of the files that we want to
        // read are kernel seqfiles, which always have a length of zero. So we have
        // to read as much as we can into a buffer.
        let buf_size = 1024 - 2 * std::mem::size_of::<usize>() as u64;

        let mut file = std::fs::File::open(std::path::PathBuf::from(filename))?.take(buf_size);
        let mut content = Vec::new();
        file.read_to_end(&mut content)?;

        let section = MemoryArrayWriter::<u8>::alloc_from_array(buffer, &content)?;
        Ok(section.location())
    }
}
