use crate::OutputFileData;
use crate::alignment::MACHO_PAGE_ALIGNMENT;
use crate::bail;
use crate::elf::get_page_mask;
use crate::ensure;
use crate::error;
use crate::error::Context;
use crate::error::Result;
use crate::file_writer::SizedOutput;
use crate::file_writer::split_buffers_by_alignment;
use crate::file_writer::split_output_by_group;
use crate::file_writer::split_output_into_sections;
use crate::layout::EpilogueLayout;
use crate::layout::FileLayout;
use crate::layout::Layout;
use crate::layout::ObjectLayout;
use crate::layout::OutputRecordLayout;
use crate::layout::PreludeLayout;
use crate::layout::Resolution;
use crate::layout::Section;
use crate::layout::SymbolCopyInfo;
use crate::macho::BuildVersionCommand;
use crate::macho::CHAINED_FIXUP_PAGE_START_SIZE;
use crate::macho::CS_BLOB_HEADERS_SIZE;
use crate::macho::CS_BLOCK_SIZE;
use crate::macho::CS_BLOCK_SIZE_EXP;
use crate::macho::CS_CODE_DIRECTORY_SIZE;
use crate::macho::CS_HASH_SIZE;
use crate::macho::CS_HEADERS_SIZE;
use crate::macho::ChainedFixupsHeader;
use crate::macho::ChainedStartsInSegment;
use crate::macho::CodeSignatureCommand;
use crate::macho::DYLINKER_PATH;
use crate::macho::DyldChainedFixupsCommand;
use crate::macho::DylibCommand;
use crate::macho::DylinkerCommand;
use crate::macho::EntryPointCommand;
use crate::macho::FileHeader;
use crate::macho::GOT_ENTRY_SIZE;
use crate::macho::MACHO_COMMAND_ALIGNMENT;
use crate::macho::MACHO_START_MEM_ADDRESS;
use crate::macho::MAX_CHAINED_FIXUP_PAGES_PER_SEGMENT;
use crate::macho::MAX_SEGMENT_COUNT;
use crate::macho::MachO;
use crate::macho::PLT_ENTRY_SIZE;
use crate::macho::SectionEntry;
use crate::macho::SectionFlags;
use crate::macho::SegmentCommand;
use crate::macho::SegmentName;
use crate::macho::SymtabCommand;
use crate::macho::UuidCommand;
use crate::macho::code_signature_identifier;
use crate::macho::code_signature_padded_identifier_size;
use crate::macho::get_segment_sections;
use crate::macho::load_dylib_command_size;
use crate::macho::output_section_id;
use crate::macho::output_section_id::LOAD_COMMANDS;
use crate::macho::part_id;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::SectionName;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::output_trace::HexU64;
use crate::output_trace::TraceOutput;
use crate::platform::Arch;
use crate::platform::Args;
use crate::platform::ObjectFile;
use crate::platform::Relaxation;
use crate::platform::SectionAttributes as _;
use crate::platform::Symbol;
use crate::resolution::SectionSlot;
use crate::symbol_db::SymbolId;
use crate::timing_phase;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use itertools::Itertools;
use linker_utils::elf::RelocationKind;
use linker_utils::utils::slice_from_all_bytes_mut;
use object::BigEndian;
use object::Endianness;
use object::SymbolIndex;
use object::U16;
use object::U32;
use object::from_bytes_mut;
use object::macho;
use object::macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12;
use object::macho::CPU_SUBTYPE_ARM64_ALL;
use object::macho::CPU_TYPE_ARM64;
use object::macho::CS_ADHOC;
use object::macho::CS_EXECSEG_MAIN_BINARY;
use object::macho::CS_HASHTYPE_SHA256;
use object::macho::CS_LINKER_SIGNED;
use object::macho::CS_SUPPORTSEXECSEG;
use object::macho::CSSLOT_CODEDIRECTORY;
use object::macho::DYLD_CHAINED_IMPORT;
use object::macho::DYLD_CHAINED_PTR_64_OFFSET;
use object::macho::LC_BUILD_VERSION;
use object::macho::LC_CODE_SIGNATURE;
use object::macho::LC_DYLD_CHAINED_FIXUPS;
use object::macho::LC_DYLD_EXPORTS_TRIE;
use object::macho::LC_LOAD_DYLIB;
use object::macho::LC_LOAD_DYLINKER;
use object::macho::LC_MAIN;
use object::macho::LC_SEGMENT_64;
use object::macho::LC_SYMTAB;
use object::macho::LC_UUID;
use object::macho::LoadCommand;
use object::macho::MH_CIGAM_64;
use object::macho::MH_EXECUTE;
use object::macho::N_ABS;
use object::macho::N_SECT;
use object::macho::N_UNDF;
use object::macho::PLATFORM_MACOS;
use object::macho::RelocationInfo;
use object::macho::SegmentFlags;
use object::slice_from_bytes_mut;
use object::write::macho::CodeDirectory;
use object::write::macho::CodeSignatureEncoder;
use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;
use rayon::slice::ParallelSlice;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::ops::BitAnd;
use tracing::debug_span;
use zerocopy::FromZeros;

const LE: Endianness = Endianness::Little;

type MachOLayout<'data> = Layout<'data, MachO>;
type SymtabEntry = object::macho::Nlist64<Endianness>;
type ExportsTrieCommand = object::macho::LinkeditDataCommand<Endianness>;

pub(crate) fn write<'data, A: Arch<Platform = MachO>>(
    sized_output: &mut SizedOutput<impl OutputFileData>,
    layout: &MachOLayout<'data>,
) -> Result {
    timing_phase!("Write data to file");
    let exports_trie = build_exports_trie(layout)?;
    let (mut section_buffers, mut padding) =
        split_output_into_sections(layout, &mut sized_output.out);
    padding.fill_zero();

    let mut writable_buckets = split_buffers_by_alignment(&mut section_buffers, layout);
    let groups_and_buffers = split_output_by_group(layout, &mut writable_buckets);
    groups_and_buffers
        .into_par_iter()
        .try_for_each(|(group, mut buffers)| -> Result {
            verbose_timing_phase!("Write group");

            let mut symbol_writer = MachOSymbolTableWriter {
                next_strtab_offset: group.strtab_start_offset,
            };
            for file in &group.files {
                write_file::<A>(
                    file,
                    &mut buffers,
                    layout,
                    &sized_output.trace,
                    &mut symbol_writer,
                    &exports_trie,
                )
                .with_context(|| format!("Failed copying from {file} to output file"))?;
            }
            Ok(())
        })?;

    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    write_got_entries(layout, section_buffers.get_mut(output_section_id::GOT))?;
    write_plt_entries::<A>(layout, section_buffers.get_mut(output_section_id::PLT_GOT))?;
    drop(section_buffers);

    let fixups = collect_chained_fixups(layout, &sized_output.out)?;
    apply_chained_fixups(layout, &fixups, &mut sized_output.out)?;
    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    write_chained_fixup_table(
        layout,
        &fixups,
        section_buffers.get_mut(output_section_id::CHAINED_FIXUP_TABLE),
    )?;

    write_code_signature_metadata(layout, sized_output)?;
    write_uuid(layout, sized_output)?;
    write_code_signature_hashes(layout, sized_output)?;

    Ok(())
}

fn write_file<'data, A: Arch<Platform = MachO>>(
    file: &FileLayout<'data, MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    _trace: &TraceOutput,
    symbol_writer: &mut MachOSymbolTableWriter,
    exports_trie: &[u8],
) -> Result {
    match file {
        FileLayout::Object(s) => {
            write_object::<A>(s, buffers, layout, symbol_writer)?;
        }
        FileLayout::Prelude(s) => write_prelude(s, buffers, layout, exports_trie)?,
        FileLayout::Epilogue(s) => write_epilogue(s, buffers, layout, exports_trie)?,
        _ => {
            // TODO
        }
    }
    Ok(())
}

/// Takes enough bytes from `bytes` for a T, returning those bytes as an `&mut T`.
fn take_mut<'out, T: object::Pod>(bytes: &mut &'out mut [u8]) -> Result<&'out mut T> {
    let bytes = bytes
        .split_off_mut(..size_of::<T>())
        .context("Insufficient allocation")?;
    from_bytes_mut::<T>(bytes)
        .map_err(|()| error!("Unaligned write"))
        .map(|(a, _)| a)
}

fn write_prelude<'data>(
    prelude: &PreludeLayout<MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    exports_trie: &[u8],
) -> Result {
    verbose_timing_phase!("Write prelude");
    debug_assert_eq!(
        prelude.format_specific.imported_library_file_ids.len(),
        prelude.format_specific.load_dylib_command_sizes.len()
    );

    let header_buffer = buffers.get_mut(crate::part_id::FILE_HEADER);
    populate_file_header(layout, prelude, take_mut(header_buffer)?);
    ensure!(header_buffer.is_empty(), "Excess FILE_HEADER allocation");

    let mut load_command_buffer = slice_from_all_bytes_mut(buffers.get_mut(part_id::LOAD_COMMANDS));
    write_segment_commands(layout, &mut load_command_buffer)?;

    if layout.symbol_db.output_kind.is_executable() {
        write_entry_point_command(layout, take_mut(&mut load_command_buffer)?)?;
    }

    write_uuid_command(take_mut(&mut load_command_buffer)?);

    if layout.args().platform_version.is_some() {
        let build_version_command = take_mut(&mut load_command_buffer)?;
        write_build_version_command(layout, build_version_command)?;
    }

    let command_size = (size_of::<DylinkerCommand>() + DYLINKER_PATH.len())
        .next_multiple_of(MACHO_COMMAND_ALIGNMENT);
    let mut command_buffer = load_command_buffer.split_off_mut(..command_size).unwrap();
    let dylinker_command = take_mut(&mut command_buffer)?;
    write_dylinker_command(dylinker_command, command_buffer);

    for (&file_id, &command_size) in prelude
        .format_specific
        .imported_library_file_ids
        .iter()
        .zip(&prelude.format_specific.load_dylib_command_sizes)
    {
        let mut command_buffer = load_command_buffer.split_off_mut(..command_size).unwrap();
        let dylib_command = take_mut(&mut command_buffer)?;
        let path = crate::macho::install_name(file_id, &layout.symbol_db);

        write_dylib_command(dylib_command, command_buffer, path);
    }

    write_dyld_chained_fixups_command(layout, take_mut(&mut load_command_buffer)?);

    if layout.symbol_db.output_kind.needs_dynsym() {
        write_exports_trie_command(layout, exports_trie, take_mut(&mut load_command_buffer)?)?;
    }

    write_symtab_command(layout, take_mut(&mut load_command_buffer)?);

    write_code_signature_command(layout, take_mut(&mut load_command_buffer)?);

    ensure!(
        load_command_buffer.is_empty(),
        "Excess LOAD_COMMANDS allocation"
    );

    // Fill up one extra character as n_strx == 0 is treated as unnamed.
    buffers.get_mut(part_id::STRTAB).fill(0);

    Ok(())
}

fn write_epilogue(
    _epilogue: &EpilogueLayout<MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    _layout: &MachOLayout<'_>,
    exports_trie: &[u8],
) -> Result {
    verbose_timing_phase!("Write epilogue");
    buffers.get_mut(part_id::CHAINED_FIXUP_TABLE).fill(0);
    let out = buffers.get_mut(part_id::EXPORTS_TRIE);
    ensure!(
        exports_trie.len() <= out.len(),
        "Mach-O exports trie exceeded its reserved size"
    );
    out[..exports_trie.len()].copy_from_slice(exports_trie);
    out[exports_trie.len()..].fill(0);

    Ok(())
}

fn build_exports_trie(layout: &MachOLayout<'_>) -> Result<Vec<u8>> {
    if !layout.symbol_db.output_kind.needs_dynsym() {
        return Ok(Vec::new());
    }

    let text_segment = layout
        .segment_layouts
        .segments
        .iter()
        .find(|segment| layout.program_segments.segment_def(segment.id).name == SegmentName::TEXT)
        .context("Missing Mach-O __TEXT segment")?;

    let image_base = text_segment.sizes.mem_offset;

    let mut symbols = layout
        .dynamic_symbol_definitions
        .iter()
        .filter_map(|symbol| {
            let resolution = layout
                .symbol_resolutions
                .get(symbol.symbol_id)
                .with_context(|| {
                    format!(
                        "Missing resolution for exported symbol `{}`",
                        String::from_utf8_lossy(symbol.name)
                    )
                });
            let resolution = match resolution {
                Ok(resolution) => resolution,
                Err(error) => return Some(Err(error)),
            };

            // Archive members can contain global symbols whose sections were not selected. They
            // have no address and must not appear in the executable's exports trie.
            if !resolution.is_absolute() && resolution.raw_value < image_base {
                return None;
            }

            Some((|| -> Result<_> {
                let (address, mut flags) = if resolution.is_absolute() {
                    (
                        resolution.raw_value,
                        object::macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE.into(),
                    )
                } else {
                    (
                        resolution
                            .raw_value
                            .checked_sub(image_base)
                            .context("exported symbol is before the Mach-O image base")?,
                        object::macho::ExportSymbolFlags(0),
                    )
                };

                if exported_symbol_is_weak(layout, symbol.symbol_id)? {
                    flags |= object::macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION;
                }

                Ok(crate::trie::Symbol {
                    name: symbol.name,
                    address,
                    flags,
                })
            })())
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(crate::trie::build(&mut symbols))
}

fn exported_symbol_is_weak(layout: &MachOLayout<'_>, symbol_id: SymbolId) -> Result<bool> {
    let file_id = layout.symbol_db.file_id_for_symbol(symbol_id);
    let FileLayout::Object(object) = layout.file_layout(file_id) else {
        return Ok(false);
    };
    let symbol_index = object.symbol_id_range.id_to_input(symbol_id);
    Ok(object.object.symbol(symbol_index)?.is_weak())
}

fn write_got_entries(layout: &MachOLayout<'_>, got: &mut [u8]) -> Result {
    let got_layout = layout.section_layouts.get(output_section_id::GOT);

    enum GotEntry<'a> {
        Import(usize, &'a crate::macho::ImportedSymbolWithResolution),
        Local(&'a crate::macho::LocalGotEntry),
    }

    let mut entries = layout
        .format_specific
        .imported_symbols
        .iter()
        .enumerate()
        .map(|(ordinal, symbol)| GotEntry::Import(ordinal, symbol))
        .chain(
            layout
                .format_specific
                .local_got_entries
                .iter()
                .map(GotEntry::Local),
        )
        .collect_vec();
    entries.sort_by_key(|entry| match entry {
        GotEntry::Import(_, symbol) => symbol.got_address,
        GotEntry::Local(entry) => entry.got_address,
    });

    for (i, entry) in entries.iter().enumerate() {
        let got_address = match entry {
            GotEntry::Import(_, symbol) => symbol.got_address,
            GotEntry::Local(entry) => entry.got_address,
        };
        let offset = got_address
            .get()
            .checked_sub(got_layout.mem_offset)
            .ok_or_else(|| error!("GOT entry address is before __got"))?
            as usize;
        let end = offset + GOT_ENTRY_SIZE as usize;

        /* DYLD_CHAINED_PTR_64 format:
        uint64_t dyld_chained_ptr_64_bind:
          ordinal: 24
          addend: 8 // 0 thru 255
          reserved: 19 // all zeros
          next: 12 // 4-byte stride
          bind: 1 // == 1
        */
        // TODO: when crossing a page boundary, next is equal to zero
        let next = if i == entries.len() - 1 { 0 } else { 2 };
        let next = next << 51;
        let value = match entry {
            GotEntry::Import(ordinal, _) => (1u64 << 63) | next | *ordinal as u64,
            GotEntry::Local(entry) => {
                let target = entry
                    .target_address
                    .checked_sub(MACHO_START_MEM_ADDRESS)
                    .context("local GOT target is before the Mach-O image base")?;
                ensure!(
                    target < (1u64 << 36),
                    "local GOT target exceeds DYLD_CHAINED_PTR_64_OFFSET range"
                );
                next | target
            }
        };
        got[offset..end].copy_from_slice(&value.to_le_bytes());
    }

    Ok(())
}

fn write_plt_entries<A: Arch<Platform = MachO>>(
    layout: &MachOLayout<'_>,
    plt: &mut [u8],
) -> Result {
    let plt_layout = layout.section_layouts.get(output_section_id::PLT_GOT);

    for imported_symbol in &layout.format_specific.imported_symbols {
        let Some(stub_address) = imported_symbol.plt_address else {
            continue;
        };

        let offset = stub_address
            .get()
            .checked_sub(plt_layout.mem_offset)
            .ok_or_else(|| error!("STUB entry address is before __stubs"))?
            as usize;
        let end = offset + PLT_ENTRY_SIZE as usize;

        A::write_plt_entry(
            &mut plt[offset..end],
            imported_symbol.got_address.get(),
            stub_address.get(),
        )?;
    }

    Ok(())
}

fn populate_file_header(
    layout: &MachOLayout,
    prelude: &PreludeLayout<MachO>,
    header: &mut FileHeader,
) {
    let load_commands_info = layout.section_layouts.get(LOAD_COMMANDS);
    let has_tlv_descriptors = layout
        .output_sections
        .ids_with_info()
        .any(|(section_id, info)| {
            info.section_attributes.ty() == macho::S_THREAD_LOCAL_VARIABLES
                && layout.section_layouts.get(section_id).mem_size != 0
        });

    header.magic.set(BigEndian, MH_CIGAM_64);
    header.cputype.set(LE, CPU_TYPE_ARM64);
    header.cpusubtype.set(LE, CPU_SUBTYPE_ARM64_ALL.into());
    header.filetype.set(LE, MH_EXECUTE);
    header
        .ncmds
        .set(LE, prelude.format_specific.load_command_count as u32);
    header
        .sizeofcmds
        .set(LE, load_commands_info.file_size as u32);
    let mut flags = macho::MH_PIE | macho::MH_DYLDLINK | macho::MH_NOUNDEFS | macho::MH_TWOLEVEL;
    if has_tlv_descriptors {
        flags |= macho::MH_HAS_TLV_DESCRIPTORS;
    }
    header.flags.set(LE, flags);
    header.reserved.set(LE, 0);
}

fn split_segment_command_buffer(
    mut bytes: &mut [u8],
    section_count: usize,
) -> Result<(&mut SegmentCommand, &mut [SectionEntry])> {
    let command = take_mut(&mut bytes)?;
    let (sections, rest) = slice_from_bytes_mut(bytes, section_count)
        .map_err(|_| error!("Invalid segment section allocation"))?;
    ensure!(
        rest.is_empty(),
        "Trailing bytes in segment command allocation"
    );
    Ok((command, sections))
}

fn write_segment_commands(layout: &MachOLayout, load_commands: &mut &mut [u8]) -> Result {
    let load_cmd_err = |()| error!("Invalid LOAD_COMMANDS allocation");
    let pagezero_segment = take_mut(load_commands)?;
    write_segment(
        SegmentName::PAGEZERO,
        macho::VmProt(0),
        pagezero_segment,
        0,
        0,
        0,
        MACHO_START_MEM_ADDRESS,
        0,
        SegmentFlags::default(),
    );

    for segment_layout in &layout.segment_layouts.segments {
        let segment_id = segment_layout.id;
        let segment_def = *layout.program_segments.segment_def(segment_id);

        let segment_sections = get_segment_sections(layout, segment_id);
        let section_count = segment_sections.len();
        let command_size = size_of::<SegmentCommand>() + size_of::<SectionEntry>() * section_count;

        let (segment, sections) = split_segment_command_buffer(
            load_commands
                .split_off_mut(..command_size)
                .ok_or_else(|| load_cmd_err(()))?,
            section_count,
        )?;

        let size = segment_layout.sizes;
        write_segment(
            segment_def.name,
            segment_def.prot,
            segment,
            size.file_offset as u64,
            size.file_size as u64,
            size.mem_offset,
            size.mem_size,
            section_count,
            segment_def.flags,
        );
        write_sections(segment_def.name, sections, &segment_sections);
    }

    Ok(())
}

fn write_segment(
    seg_name: SegmentName,
    prot_flags: object::macho::VmProt,
    segment_cmd: &mut SegmentCommand,
    file_offset: u64,
    file_size: u64,
    mem_offset: u64,
    mem_size: u64,
    section_count: usize,
    flags: macho::SegmentFlags,
) {
    segment_cmd.cmd.set(LE, LC_SEGMENT_64);
    segment_cmd.cmdsize.set(
        LE,
        (size_of::<SegmentCommand>() + size_of::<SectionEntry>() * section_count) as u32,
    );
    segment_cmd.segname = seg_name.into_bytes();
    segment_cmd.fileoff.set(LE, file_offset);
    segment_cmd.filesize.set(LE, file_size);
    segment_cmd.vmaddr.set(LE, mem_offset);
    segment_cmd.vmsize.set(LE, mem_size);
    segment_cmd.maxprot.set(LE, prot_flags);
    segment_cmd.initprot.set(LE, prot_flags);
    segment_cmd.nsects.set(LE, section_count as u32);
    segment_cmd.flags.set(LE, flags);
}

fn write_sections(
    seg_name: SegmentName,
    sections: &mut [SectionEntry],
    segment_sections: &[(
        OutputRecordLayout,
        SectionName<'_>,
        crate::macho::SectionFlags,
    )],
) {
    for (section, (size, section_name, section_flags)) in sections.iter_mut().zip(segment_sections)
    {
        let section_name = section_name.0;

        section.segname = seg_name.into_bytes();
        section.sectname[..section_name.len()].copy_from_slice(section_name);
        section.sectname[section_name.len()..].zero();
        section.addr.set(LE, size.mem_offset);
        section.size.set(LE, size.mem_size);
        section.offset.set(LE, size.file_offset as u32);
        section.align.set(LE, u32::from(size.alignment.exponent));
        section.reloff.set(LE, 0);
        section.nreloc.set(LE, 0);
        section.flags.set(LE, *section_flags);
        section.reserved1.set(LE, 0);
        // TODO: find a better place
        let reserved2 =
            if section_flags.0 & macho::SECTION_TYPE == u32::from(macho::S_SYMBOL_STUBS.0) {
                PLT_ENTRY_SIZE as u32
            } else {
                0
            };
        section.reserved2.set(LE, reserved2);
        section.reserved3.set(LE, 0);
    }
}

fn write_object<'data, A: Arch<Platform = MachO>>(
    object: &ObjectLayout<'data, MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    symbol_writer: &mut MachOSymbolTableWriter,
) -> Result {
    verbose_timing_phase!("Write object", file_id = object.file_id.as_u32());

    let _span = debug_span!("write_file", filename = %object.input).entered();
    let _file_span = layout.args().common().trace_span_for_file(object.file_id);
    for (i, sec) in object.sections.iter().enumerate() {
        match sec {
            SectionSlot::Loaded(sec) => {
                write_object_section::<A>(object, layout, *sec, object::SectionIndex(i), buffers)?;
            }
            _ => (),
        }
    }

    write_thunks::<A>(object, buffers, layout)?;

    write_symbols(object, buffers, layout, symbol_writer)?;

    Ok(())
}

fn write_object_section<'data, A: Arch<Platform = MachO>>(
    object_layout: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout<'data>,
    section: Section,
    section_index: object::SectionIndex,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
) -> Result {
    let out = write_section_raw(object_layout, layout, section, section_index, buffers)?;

    let section_address = object_layout.section_resolutions[section_index.0]
        .address()
        .context("Attempted to apply relocations to a section that we didn't load")?;

    let section_flags = object_layout.object.section(section_index)?.flags.get(LE);
    let section_part_id =
        object_layout.section_part_id(section_index, &layout.symbol_db.section_part_ids);

    let mut pending_subtractor = None;
    let mut pending_addend = None;
    for rel in object_layout.relocations(section_index)?.relocations {
        let rel = rel.info(LE);
        if rel.r_type == macho::ARM64_RELOC_ADDEND {
            ensure!(
                pending_addend.is_none(),
                "consecutive ARM64_RELOC_ADDEND relocations"
            );
            pending_addend = Some(
                i64::from(rel.r_symbolnum)
                    .wrapping_shl(64 - 24)
                    .wrapping_shr(64 - 24),
            );
            continue;
        }
        if rel.r_type == macho::ARM64_RELOC_SUBTRACTOR {
            ensure!(
                pending_subtractor.replace(rel).is_none(),
                "consecutive ARM64_RELOC_SUBTRACTOR relocations"
            );
            continue;
        }

        if let Some(subtractor) = pending_subtractor.take() {
            apply_subtractor_pair(object_layout, subtractor, rel, layout, out)?;
            continue;
        }

        apply_relocation::<A>(
            object_layout,
            section_part_id,
            section_address,
            section_flags,
            rel,
            pending_addend.take().unwrap_or(0),
            layout,
            out,
        )?;
    }
    ensure!(
        pending_subtractor.is_none(),
        "ARM64_RELOC_SUBTRACTOR is missing its paired ARM64_RELOC_UNSIGNED"
    );
    ensure!(
        pending_addend.is_none(),
        "ARM64_RELOC_ADDEND is missing its paired relocation"
    );

    Ok(())
}

fn apply_subtractor_pair<'data>(
    object_layout: &ObjectLayout<'data, MachO>,
    subtractor: RelocationInfo,
    minuend: RelocationInfo,
    layout: &MachOLayout<'data>,
    out: &mut [u8],
) -> Result {
    ensure!(
        minuend.r_type == macho::ARM64_RELOC_UNSIGNED
            && minuend.r_address == subtractor.r_address
            && minuend.r_length == subtractor.r_length,
        "ARM64_RELOC_SUBTRACTOR must be followed by a matching ARM64_RELOC_UNSIGNED"
    );

    let subtrahend = subtractor_operand_value(subtractor, object_layout, layout)?;
    let minuend_value = subtractor_operand_value(minuend, object_layout, layout)?;
    let offset = minuend.r_address as usize;
    let size = 1usize << minuend.r_length;
    let bytes = out
        .get(offset..offset + size)
        .context("subtractor relocation is outside its section")?;
    let addend = match size {
        4 => i64::from(i32::from_le_bytes(bytes.try_into().unwrap())),
        8 => i64::from_le_bytes(bytes.try_into().unwrap()),
        _ => bail!("unsupported ARM64_RELOC_SUBTRACTOR size: {size} bytes"),
    };
    let value = minuend_value
        .wrapping_sub(subtrahend)
        .wrapping_add_signed(addend);

    match size {
        4 => out[offset..offset + size].copy_from_slice(&(value as u32).to_le_bytes()),
        8 => out[offset..offset + size].copy_from_slice(&value.to_le_bytes()),
        _ => unreachable!(),
    }
    Ok(())
}

fn subtractor_operand_value<'data>(
    rel: RelocationInfo,
    object_layout: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout<'data>,
) -> Result<u64> {
    if rel.r_extern {
        return Ok(get_resolution(rel, object_layout, layout)?.0.raw_value);
    }
    if rel.r_symbolnum == macho::R_ABS {
        return Ok(0);
    }

    let section_index = rel.r_symbolnum as usize - 1;
    object_layout
        .section_resolutions
        .get(section_index)
        .context("subtractor relocation refers to an invalid section ordinal")?
        .address()
        .context("subtractor relocation refers to a section without an address")
}

#[inline(always)]
fn apply_relocation<'data, A: Arch<Platform = MachO>>(
    object_layout: &ObjectLayout<'data, MachO>,
    section_part_id: crate::part_id::PartId,
    section_address: u64,
    section_flags: SectionFlags,
    rel: RelocationInfo,
    mut addend: i64,
    layout: &MachOLayout<'data>,
    out: &mut [u8],
) -> Result {
    let mut offset_in_section = u64::from(rel.r_address);
    let place = section_address + offset_in_section;

    let _span = tracing::trace_span!(
        "relocation",
        address = place,
        address_hex = %HexU64::new(place)
    )
    .entered();

    let (resolution, _symbol_index, local_symbol_id) = get_resolution(rel, object_layout, layout)?;
    let flags = layout.flags_for_symbol(local_symbol_id);
    let output_kind = layout.symbol_db.output_kind;

    // TODO: We don't support relaxation deltas or previous relocations yet.
    let relaxation = A::new_relaxation(
        rel,
        out,
        offset_in_section,
        flags,
        output_kind,
        section_flags,
        None,
        resolution.raw_value,
        section_address,
        addend,
        None,
    );

    let rel_info = match relaxation.as_ref() {
        Some(relaxation) => {
            relaxation.apply(out, &mut offset_in_section, &mut addend);
            relaxation.rel_info()
        }
        None if rel.r_type == ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => {
            bail!(
                "TLV relocations are currently only supported for locally-defined, strong, and \
                non-interposable symbols in executables"
            )
        }
        None => A::relocation_from_raw(rel)?,
    };

    let mask = get_page_mask(rel_info.mask);
    let relocation_target = match rel_info.kind {
        RelocationKind::GotRelative | RelocationKind::Got => resolution
            .format_specific
            .got_address
            .context("missing GOT address for relocation")?
            .get(),
        RelocationKind::Relative if rel.r_type == macho::ARM64_RELOC_BRANCH26 => resolution
            .format_specific
            .plt_address
            .map_or(resolution.raw_value, |address| address.get()),
        _ => resolution.raw_value,
    };
    let symbol_plus_addend = relocation_target.wrapping_add_signed(addend);
    let is_tlv_template_offset = section_flags.0 & macho::SECTION_TYPE
        == u32::from(macho::S_THREAD_LOCAL_VARIABLES.0)
        && offset_in_section % 24 == 16;
    let mut value = match rel_info.kind {
        RelocationKind::Absolute if is_tlv_template_offset => {
            let tls_start = macho_tls_start(layout).context("TLV descriptor requires TLS data")?;
            symbol_plus_addend
                .checked_sub(tls_start)
                .context("TLV template symbol is before the TLS segment")?
        }
        RelocationKind::Absolute => symbol_plus_addend.bitand(mask.symbol_plus_addend),
        RelocationKind::AbsoluteLowPart => symbol_plus_addend.bitand(mask.symbol_plus_addend),
        RelocationKind::Relative => symbol_plus_addend
            .bitand(mask.symbol_plus_addend)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::GotRelative => symbol_plus_addend
            .bitand(mask.symbol_plus_addend)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::Got => symbol_plus_addend.bitand(mask.symbol_plus_addend),
        _ => todo!(),
    };

    if let Some(thunked_value) = maybe_get_thunk_for_relocation::<A>(
        object_layout,
        section_part_id,
        layout,
        rel_info,
        local_symbol_id,
        place,
        value,
    )? {
        value = thunked_value;
    }

    tracing::trace!(
            %flags,
            ?rel_info.kind,
            %rel_info.size,
            value,
            value_hex = %HexU64::new(value),
            symbol_name = %layout.symbol_db.symbol_name_for_display(local_symbol_id),
            "relocation applied");

    rel_info
        .write_to_buffer(value, &mut out[offset_in_section as usize..])
        .with_context(|| {
            format!(
                "Failed to apply relocation {} to {}",
                A::rel_type_to_string(rel),
                layout.symbol_debug(local_symbol_id)
            )
        })?;

    Ok(())
}

fn maybe_get_thunk_for_relocation<A: Arch<Platform = MachO>>(
    object_layout: &ObjectLayout<MachO>,
    section_part_id: crate::part_id::PartId,
    layout: &MachOLayout,
    rel_info: linker_utils::elf::RelocationKindInfo,
    local_symbol_id: SymbolId,
    place: u64,
    value: u64,
) -> Result<Option<u64>> {
    let Some(config) = A::thunk_config() else {
        return Ok(None);
    };
    if !rel_info.thunkable || rel_info.range.contains(value as i64) {
        return Ok(None);
    }

    let canonical_id = layout.symbol_db.definition(local_symbol_id);
    let thunk_id = if section_part_id == config.primary_function_part_id {
        object_layout.thunk_block_id
    } else {
        crate::thunks::ThunkBlockId::FIRST
    };
    let thunk_address = layout
        .thunk_block_addresses
        .get(thunk_id.as_usize())
        .and_then(|addresses| addresses.get(&canonical_id))
        .copied()
        .with_context(|| {
            format!(
                "Branch relocation to {} is out of range but has no thunk (source {}, primary {}, target {}, block {})",
                layout.symbol_db.symbol_name_for_display(local_symbol_id),
                layout.output_sections.part_debug(section_part_id),
                layout.output_sections.part_debug(config.primary_function_part_id),
                layout.output_sections.part_debug(
                    layout.symbol_db.part_id_for_symbol(canonical_id),
                ),
                thunk_id.as_usize(),
            )
        })?;

    let mask = get_page_mask(rel_info.mask);
    Ok(Some(
        thunk_address
            .wrapping_add(rel_info.bias)
            .bitand(mask.symbol_plus_addend)
            .wrapping_sub(place.bitand(mask.place)),
    ))
}

fn write_thunks<A: Arch<Platform = MachO>>(
    object: &ObjectLayout<MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout,
) -> Result {
    if !object.owns_thunk_block {
        return Ok(());
    }
    let Some(addresses) = layout
        .thunk_block_addresses
        .get(object.thunk_block_id.as_usize())
    else {
        return Ok(());
    };
    let Some(config) = A::thunk_config() else {
        return Ok(());
    };

    for (symbol_id, &thunk_address) in addresses {
        let resolution = layout
            .merged_symbol_resolution(*symbol_id)
            .with_context(|| {
                format!(
                    "No resolution for symbol {} needed by thunk",
                    layout.symbol_db.symbol_name_for_display(*symbol_id)
                )
            })?;
        let target_address = resolution
            .format_specific
            .plt_address
            .map_or(resolution.raw_value, |address| address.get());
        let thunk = buffers
            .get_mut(config.primary_function_part_id)
            .split_off_mut(..config.thunk_size as usize)
            .ok_or_else(|| crate::file_writer::insufficient_allocation("Mach-O thunk space"))?;
        A::write_thunk(thunk_address, target_address, thunk);
    }

    Ok(())
}

fn macho_tls_start(layout: &MachOLayout<'_>) -> Option<u64> {
    layout
        .section_layouts
        .iter()
        .filter(|(section_id, section)| {
            let section_type =
                layout.output_sections.section_flags(*section_id).0 & macho::SECTION_TYPE;
            section.mem_size != 0
                && (section_type == u32::from(macho::S_THREAD_LOCAL_REGULAR.0)
                    || section_type == u32::from(macho::S_THREAD_LOCAL_ZEROFILL.0))
        })
        .map(|(_, section)| section.mem_offset)
        .min()
}

fn write_section_raw<'out, 'data>(
    object: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout,
    sec: Section,
    section_index: object::SectionIndex,
    buffers: &'out mut OutputSectionPartMap<&mut [u8]>,
) -> Result<&'out mut [u8]> {
    let part_id = object.section_part_id(section_index, &layout.symbol_db.section_part_ids);
    if layout
        .output_sections
        .has_data_in_file(part_id.output_section_id::<MachO>())
    {
        let section_buffer = buffers.get_mut(part_id);
        let allocation_size = sec.capacity(part_id, &layout.output_sections) as usize;
        if section_buffer.len() < allocation_size {
            bail!(
                "Insufficient space allocated to section `{}`. Tried to take {} bytes, but only {} remain",
                object.object.section_display_name(section_index),
                allocation_size,
                section_buffer.len()
            );
        }
        let out = section_buffer.split_off_mut(..allocation_size).unwrap();
        let object_section = object.object.section(section_index)?;

        let section_size = object.object.section_size(object_section)?;
        let (out, padding) = out.split_at_mut(section_size as usize);
        object.object.copy_section_data(object_section, out)?;
        padding.fill(0);
        Ok(out)
    } else {
        Ok(&mut [])
    }
}

fn get_resolution<'data>(
    rel: RelocationInfo,
    object_layout: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout,
) -> Result<(Resolution<MachO>, SymbolIndex, SymbolId)> {
    let symbol_index = SymbolIndex(rel.r_symbolnum as usize);
    let local_symbol_id = object_layout.symbol_id_range.input_to_id(symbol_index);
    let sym = object_layout.object.symbol(symbol_index)?;
    let section_index = object_layout.object.symbol_section(sym, symbol_index)?;
    let resolution = layout
        .merged_symbol_resolution(local_symbol_id)
        .or_else(|| {
            section_index.and_then(|section_index| {
                let section_address =
                    object_layout.section_resolutions[section_index.0].address()?;
                Some(Resolution {
                    raw_value: section_address,
                    dynamic_symbol_index: None,
                    flags: ValueFlags::empty(),
                    format_specific: Default::default(),
                })
            })
        })
        .with_context(|| {
            format!(
                "Missing resolution for: {}",
                layout.symbol_debug(local_symbol_id)
            )
        })?;
    Ok((resolution, symbol_index, local_symbol_id))
}

fn write_entry_point_command(layout: &MachOLayout, command: &mut EntryPointCommand) -> Result {
    let entry_name = match layout.symbol_db.entry_point() {
        crate::platform::EntryPoint::Symbol(name) => String::from_utf8_lossy(name),
        crate::platform::EntryPoint::None | crate::platform::EntryPoint::Address(_) => {
            bail!("Mach-O executable entry point must be a symbol")
        }
    };

    let entry_address = layout
        .resolved_entry_symbol_address()?
        .with_context(|| format!("entry symbol `{entry_name}` is not defined"))?;

    let image_base = layout
        .section_layouts
        .get(crate::output_section_id::FILE_HEADER)
        .mem_offset;

    let entry_offset = entry_address
        .checked_sub(image_base)
        .context("entry point is before the Mach-O image base")?;

    command.cmd.set(LE, LC_MAIN);
    command
        .cmdsize
        .set(LE, size_of::<EntryPointCommand>() as u32);
    command.entryoff.set(LE, entry_offset);
    command.stacksize.set(LE, 0);
    Ok(())
}

fn write_build_version_command(layout: &MachOLayout, command: &mut BuildVersionCommand) -> Result {
    let platform_version = layout
        .args()
        .platform_version
        .as_ref()
        .ok_or("platform_version must be set")?;

    command.cmd.set(LE, LC_BUILD_VERSION);
    command
        .cmdsize
        .set(LE, size_of::<BuildVersionCommand>() as u32);
    command.platform.set(LE, PLATFORM_MACOS);
    command
        .minos
        .set(LE, platform_version.minimum_version.get());
    command.sdk.set(LE, platform_version.sdk_version.get());
    command.ntools.set(LE, 0);
    // TODO: We could record jdxld's version here, but Mach-O only defines tool IDs
    // for Apple toolchain components, so leave the tools list empty for now.
    Ok(())
}

fn write_uuid_command(command: &mut UuidCommand) {
    command.cmd.set(LE, LC_UUID);
    command.cmdsize.set(LE, size_of::<UuidCommand>() as u32);
    command.uuid.zero();
}

fn write_dylinker_command(command: &mut DylinkerCommand, path_buffer: &mut [u8]) {
    command.cmd.set(LE, LC_LOAD_DYLINKER);
    command.cmdsize.set(
        LE,
        ((size_of::<DylinkerCommand>() + DYLINKER_PATH.len())
            .next_multiple_of(MACHO_COMMAND_ALIGNMENT)) as u32,
    );
    command
        .name
        .offset
        .set(LE, size_of::<DylinkerCommand>() as u32);

    path_buffer[0..DYLINKER_PATH.len()].copy_from_slice(DYLINKER_PATH);
    path_buffer[DYLINKER_PATH.len()..].zero();
}

fn write_dylib_command(command: &mut DylibCommand, path_buffer: &mut [u8], path: &[u8]) {
    command.cmd.set(LE, LC_LOAD_DYLIB);
    command
        .cmdsize
        .set(LE, load_dylib_command_size(path) as u32);
    command
        .dylib
        .name
        .offset
        .set(LE, size_of::<DylibCommand>() as u32);
    // TODO
    command.dylib.timestamp.set(LE, 2);
    // TODO
    command
        .dylib
        .current_version
        .set(LE, macho::Version(1356 << 16));
    command
        .dylib
        .compatibility_version
        .set(LE, macho::Version(1 << 16));

    path_buffer[0..path.len()].copy_from_slice(path);
    path_buffer[path.len()..].zero();
}

fn write_dyld_chained_fixups_command(layout: &MachOLayout, command: &mut DyldChainedFixupsCommand) {
    let chained_fixup_table = layout
        .section_layouts
        .get(output_section_id::CHAINED_FIXUP_TABLE);

    command.cmd.set(LE, LC_DYLD_CHAINED_FIXUPS);
    command
        .cmdsize
        .set(LE, size_of::<DyldChainedFixupsCommand>() as u32);
    command
        .dataoff
        .set(LE, chained_fixup_table.file_offset as u32);
    command
        .datasize
        .set(LE, chained_fixup_table.file_size as u32);
}

fn write_exports_trie_command(
    layout: &MachOLayout,
    exports_trie: &[u8],
    command: &mut ExportsTrieCommand,
) -> Result {
    let exports_trie_layout = layout.section_layouts.get(output_section_id::EXPORTS_TRIE);

    command.cmd.set(LE, LC_DYLD_EXPORTS_TRIE);
    command
        .cmdsize
        .set(LE, size_of::<ExportsTrieCommand>() as u32);
    command.dataoff.set(
        LE,
        exports_trie_layout
            .file_offset
            .try_into()
            .context("Mach-O exports trie offset exceeds 32 bits")?,
    );
    command.datasize.set(
        LE,
        exports_trie
            .len()
            .try_into()
            .context("Mach-O exports trie size exceeds 32 bits")?,
    );
    Ok(())
}

fn write_symtab_command(layout: &MachOLayout, command: &mut SymtabCommand) {
    let symtab = layout.section_layouts.get(output_section_id::SYMTAB_GLOBAL);
    let strtab = layout.section_layouts.get(output_section_id::STRTAB);

    command.cmd.set(LE, LC_SYMTAB);
    command.cmdsize.set(LE, size_of::<SymtabCommand>() as u32);
    command.symoff.set(LE, symtab.file_offset as u32);
    command
        .nsyms
        .set(LE, (symtab.file_size / size_of::<SymtabEntry>()) as u32);
    command.stroff.set(LE, strtab.file_offset as u32);
    command.strsize.set(LE, strtab.file_size as u32);
}

fn write_code_signature_command(layout: &MachOLayout, command: &mut CodeSignatureCommand) {
    let code_signature = layout
        .section_layouts
        .get(output_section_id::CODE_SIGNATURE);

    command.cmd.set(LE, LC_CODE_SIGNATURE);
    command
        .cmdsize
        .set(LE, size_of::<CodeSignatureCommand>() as u32);
    command.dataoff.set(LE, code_signature.file_offset as u32);
    command.datasize.set(LE, code_signature.file_size as u32);
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ChainedFixupTarget {
    Rebase(u64),
    Bind { ordinal: u32, addend: u8 },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct ChainedFixup {
    segment_index: usize,
    address: u64,
    file_offset: usize,
    target: ChainedFixupTarget,
}

fn collect_chained_fixups(layout: &MachOLayout<'_>, output: &[u8]) -> Result<Vec<ChainedFixup>> {
    let import_ordinals = layout
        .format_specific
        .imported_symbols
        .iter()
        .enumerate()
        .map(|(ordinal, symbol)| {
            (
                layout.symbol_db.definition(symbol.symbol_id),
                ordinal as u32,
            )
        })
        .collect::<HashMap<_, _>>();
    let mut fixups = Vec::new();

    for group in &layout.group_layouts {
        for file in &group.files {
            let FileLayout::Object(object) = file else {
                continue;
            };
            for (section_number, section) in object.sections.iter().enumerate() {
                let SectionSlot::Loaded(_) = section else {
                    continue;
                };
                let section_index = object::SectionIndex(section_number);
                let Some(section_address) = object.section_resolutions[section_number].address()
                else {
                    continue;
                };
                let section_flags = object.object.section(section_index)?.flags.get(LE);
                let is_tlv_descriptors = section_flags.0 & macho::SECTION_TYPE
                    == u32::from(macho::S_THREAD_LOCAL_VARIABLES.0);
                let mut pending_subtractor = false;

                for relocation in object.relocations(section_index)?.relocations {
                    let relocation = relocation.info(LE);
                    if relocation.r_type == macho::ARM64_RELOC_SUBTRACTOR {
                        pending_subtractor = true;
                        continue;
                    }
                    if pending_subtractor {
                        pending_subtractor = false;
                        continue;
                    }
                    if relocation.r_type == macho::ARM64_RELOC_ADDEND {
                        continue;
                    }
                    if relocation.r_type != macho::ARM64_RELOC_UNSIGNED
                        || relocation.r_length != 3
                        || relocation.r_pcrel
                    {
                        continue;
                    }

                    let offset = u64::from(relocation.r_address);
                    if is_tlv_descriptors && offset % 24 == 16 {
                        // The third TLV descriptor field is an offset into the TLS template, not a
                        // pointer. It was converted while applying relocations.
                        continue;
                    }

                    let address = section_address + offset;
                    let Some((segment_index, file_offset)) = fixup_location(layout, address) else {
                        continue;
                    };
                    let bytes = output
                        .get(file_offset..file_offset + size_of::<u64>())
                        .context("chained fixup is outside the output file")?;
                    let relocated_value = u64::from_le_bytes(bytes.try_into().unwrap());
                    let (resolution, _, local_symbol_id) =
                        get_resolution(relocation, object, layout)?;
                    let target = if resolution.flags.is_dynamic() {
                        let canonical_id = layout.symbol_db.definition(local_symbol_id);
                        let ordinal = *import_ordinals.get(&canonical_id).with_context(|| {
                            format!(
                                "missing chained import for {}",
                                layout.symbol_debug(local_symbol_id)
                            )
                        })?;
                        let addend = relocated_value.wrapping_sub(resolution.raw_value);
                        ensure!(
                            addend <= u8::MAX.into(),
                            "chained bind addend for {} exceeds 8 bits",
                            layout.symbol_debug(local_symbol_id)
                        );
                        ChainedFixupTarget::Bind {
                            ordinal,
                            addend: addend as u8,
                        }
                    } else {
                        ChainedFixupTarget::Rebase(relocated_value)
                    };
                    fixups.push(ChainedFixup {
                        segment_index,
                        address,
                        file_offset,
                        target,
                    });
                }
            }
        }
    }

    for (ordinal, symbol) in layout.format_specific.imported_symbols.iter().enumerate() {
        let address = symbol.got_address.get();
        let (segment_index, file_offset) = fixup_location(layout, address)
            .context("imported GOT entry is outside a writable segment")?;
        fixups.push(ChainedFixup {
            segment_index,
            address,
            file_offset,
            target: ChainedFixupTarget::Bind {
                ordinal: ordinal as u32,
                addend: 0,
            },
        });
    }
    for entry in &layout.format_specific.local_got_entries {
        let address = entry.got_address.get();
        let (segment_index, file_offset) = fixup_location(layout, address)
            .context("local GOT entry is outside a writable segment")?;
        fixups.push(ChainedFixup {
            segment_index,
            address,
            file_offset,
            target: ChainedFixupTarget::Rebase(entry.target_address),
        });
    }

    fixups.sort_unstable_by_key(|fixup| (fixup.segment_index, fixup.address));
    let mut deduplicated: Vec<ChainedFixup> = Vec::with_capacity(fixups.len());
    for fixup in fixups {
        if let Some(previous) = deduplicated.last()
            && previous.address == fixup.address
        {
            ensure!(
                *previous == fixup,
                "conflicting chained fixups at {:#x}",
                fixup.address
            );
            continue;
        }
        deduplicated.push(fixup);
    }
    Ok(deduplicated)
}

fn fixup_location(layout: &MachOLayout<'_>, address: u64) -> Option<(usize, usize)> {
    for (segment_index, segment) in layout.segment_layouts.segments.iter().enumerate() {
        let segment_end = segment.sizes.mem_offset + segment.sizes.file_size as u64;
        if address < segment.sizes.mem_offset || address + 8 > segment_end {
            continue;
        }
        let segment_name = layout.program_segments.segment_def(segment.id).name;
        if !segment_name.is_writable() {
            return None;
        }
        let offset_in_segment = (address - segment.sizes.mem_offset) as usize;
        return Some((segment_index, segment.sizes.file_offset + offset_in_segment));
    }
    None
}

fn apply_chained_fixups(
    layout: &MachOLayout<'_>,
    fixups: &[ChainedFixup],
    output: &mut [u8],
) -> Result {
    let image_base = layout
        .segment_layouts
        .segments
        .iter()
        .find(|segment| layout.program_segments.segment_def(segment.id).name == SegmentName::TEXT)
        .context("missing Mach-O __TEXT segment")?
        .sizes
        .mem_offset;
    let page_size = MACHO_PAGE_ALIGNMENT.value();

    for (index, fixup) in fixups.iter().enumerate() {
        let next = fixups.get(index + 1).filter(|next| {
            next.segment_index == fixup.segment_index
                && (next.address / page_size) == (fixup.address / page_size)
        });
        let next_stride = next.map_or(0, |next| (next.address - fixup.address) / 4);
        ensure!(
            next_stride < (1 << 12),
            "chained fixup stride exceeds 12 bits"
        );
        let next_bits = next_stride << 51;
        let value = match fixup.target {
            ChainedFixupTarget::Rebase(target) => {
                let runtime_offset = target
                    .checked_sub(image_base)
                    .context("chained rebase target is before the image base")?;
                ensure!(
                    runtime_offset < (1u64 << 36),
                    "chained rebase target exceeds 36 bits"
                );
                next_bits | runtime_offset | ((target >> 56) << 36)
            }
            ChainedFixupTarget::Bind { ordinal, addend } => {
                ensure!(
                    ordinal < (1 << 24),
                    "chained import ordinal exceeds 24 bits"
                );
                (1u64 << 63) | next_bits | (u64::from(addend) << 24) | u64::from(ordinal)
            }
        };
        output[fixup.file_offset..fixup.file_offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    Ok(())
}

fn write_chained_fixup_table(
    layout: &MachOLayout,
    fixups: &[ChainedFixup],
    chained_fixup_table: &mut [u8],
) -> Result {
    let symbols = &layout.format_specific.imported_symbols;
    let active_segments = &layout.segment_layouts.segments;

    // The __PAGEZERO segment needs to be added manually.
    let segment_count = active_segments.len() + 1;
    ensure!(
        segment_count <= MAX_SEGMENT_COUNT,
        "unexpected number of active segments"
    );
    let starts_in_image_len = size_of::<u32>() * (segment_count + 1);
    let fixup_segments = fixups
        .iter()
        .chunk_by(|fixup| fixup.segment_index)
        .into_iter()
        .map(|(segment_index, fixups)| -> Result<_> {
            let fixups = fixups.collect_vec();
            let segment = &active_segments[segment_index];
            let last = fixups.last().unwrap();
            let page_count = ((last.address - segment.sizes.mem_offset)
                / MACHO_PAGE_ALIGNMENT.value()
                + 1) as usize;
            ensure!(
                page_count <= MAX_CHAINED_FIXUP_PAGES_PER_SEGMENT,
                "Mach-O fixup segment exceeds the mbx development-build size limit"
            );
            Ok((segment_index, page_count, fixups))
        })
        .collect::<Result<Vec<_>>>()?;
    let starts_in_segments_len = fixup_segments
        .iter()
        .map(|(_, page_count, _)| {
            size_of::<ChainedStartsInSegment>()
                + CHAINED_FIXUP_PAGE_START_SIZE as usize * page_count
        })
        .sum::<usize>();
    let imports_len = size_of::<u32>() * symbols.len();

    let starts_offset = size_of::<ChainedFixupsHeader>();
    let imports_offset = starts_offset + starts_in_image_len + starts_in_segments_len;
    let symbols_offset = imports_offset + imports_len;
    ensure!(
        symbols_offset <= chained_fixup_table.len(),
        "Mach-O chained fixups exceeded their reserved size"
    );

    let (header, rest) = from_bytes_mut::<ChainedFixupsHeader>(chained_fixup_table)
        .map_err(|_| error!("Invalid chained fixups header allocation"))?;
    let (starts_in_image_bytes, rest) = rest.split_at_mut(starts_in_image_len);
    let (starts_in_image, _) =
        slice_from_bytes_mut::<U32<Endianness>>(starts_in_image_bytes, segment_count + 1)
            .map_err(|_| error!("Invalid chained fixups starts allocation"))?;
    let (starts_in_segments, rest) = rest.split_at_mut(starts_in_segments_len);
    let (imports, string_pool) = slice_from_bytes_mut::<U32<Endianness>>(rest, symbols.len())
        .map_err(|_| error!("Invalid chained fixups imports allocation"))?;

    // 1) fill up ChainedFixupsHeader
    header.fixups_version.set(LE, 0);
    header.starts_offset.set(LE, starts_offset as u32);
    header.imports_offset.set(LE, imports_offset as u32);
    header.symbols_offset.set(LE, symbols_offset as u32);
    header.imports_count.set(LE, symbols.len() as u32);
    header.imports_format.set(LE, DYLD_CHAINED_IMPORT);
    header.symbols_format.set(LE, 0);

    // 2) Fill dyld_chained_starts_in_image, which is `seg_count` (u32) followed by
    // `seg_info_offset` ([u32; seg_count]).
    starts_in_image[0].set(LE, segment_count as u32);
    starts_in_image[1..].fill(U32::new(LE, 0));

    // 3) Emit one starts-in-segment record for each segment that contains fixups.
    let image_base = active_segments
        .iter()
        .find(|segment| layout.program_segments.segment_def(segment.id).name == SegmentName::TEXT)
        .context("missing Mach-O __TEXT segment")?
        .sizes
        .mem_offset;
    let mut segment_bytes = starts_in_segments;
    let mut segment_info_offset = starts_in_image_len;
    for (segment_index, page_count, segment_fixups) in &fixup_segments {
        // Accounts for both seg_count and __PAGEZERO.
        starts_in_image[segment_index + 2].set(LE, segment_info_offset as u32);
        let record_len = size_of::<ChainedStartsInSegment>()
            + CHAINED_FIXUP_PAGE_START_SIZE as usize * page_count;
        let (record, remaining) = segment_bytes.split_at_mut(record_len);
        segment_bytes = remaining;
        let (starts_in_segment, rest) = from_bytes_mut::<ChainedStartsInSegment>(record)
            .map_err(|_| error!("Invalid chained fixups starts in segment allocation"))?;
        let (page_starts, _) = slice_from_bytes_mut::<U16<Endianness>>(rest, *page_count)
            .map_err(|_| error!("Invalid chained fixups page starts allocation"))?;

        let segment = &active_segments[*segment_index];
        starts_in_segment.size.set(LE, record_len as u32);
        starts_in_segment
            .page_size
            .set(LE, MACHO_PAGE_ALIGNMENT.value() as u16);
        starts_in_segment
            .pointer_format
            .set(LE, DYLD_CHAINED_PTR_64_OFFSET);
        starts_in_segment.segment_offset.set(
            LE,
            segment
                .sizes
                .mem_offset
                .checked_sub(image_base)
                .context("fixup segment is before the image base")?,
        );
        starts_in_segment.max_valid_pointer.set(LE, 0);
        starts_in_segment.page_count.set(LE, *page_count as u16);
        page_starts.fill(U16::new(LE, u16::MAX));
        for fixup in segment_fixups {
            let offset_in_segment = fixup.address - segment.sizes.mem_offset;
            let page_index = (offset_in_segment / MACHO_PAGE_ALIGNMENT.value()) as usize;
            if page_starts[page_index].get(LE) == u16::MAX {
                page_starts[page_index].set(
                    LE,
                    (offset_in_segment % MACHO_PAGE_ALIGNMENT.value()) as u16,
                );
            }
        }
        segment_info_offset += record_len;
    }
    ensure!(
        segment_bytes.is_empty(),
        "Excess starts-in-segment allocation"
    );

    let sorted_symbols = &layout.format_specific.imported_symbols;
    let mut symbol_offsets = Vec::with_capacity(sorted_symbols.len());
    let mut str_offset = 0;
    for imported_symbol in sorted_symbols {
        let symbol_name = layout
            .symbol_db
            .symbol_name(imported_symbol.symbol_id)
            .unwrap()
            .bytes();
        string_pool[str_offset..str_offset + symbol_name.len()].copy_from_slice(symbol_name);
        string_pool[str_offset + symbol_name.len()] = b'\0';
        symbol_offsets.push(str_offset);
        str_offset += symbol_name.len() + 1;
    }

    // 4) Emit `dyld_chained_import` that is built by 3 pieces:
    // lib_ordinal: 8
    // weak_import: 1
    // name_offset: 23
    for (i, imported_symbol) in sorted_symbols.iter().enumerate() {
        let file_id = layout
            .symbol_db
            .file_id_for_symbol(imported_symbol.symbol_id);

        let dynamic = match layout.file_layout(file_id) {
            FileLayout::StubLibrary(file) => &file.format_specific,
            FileLayout::Dynamic(file) => &file.format_specific,
            _ => {
                bail!("Internal error: Internal symbol refers to non-stub library");
            }
        };

        let lib_ordinal = dynamic.ordinal.get();

        imports[i].set(
            Endianness::Little,
            u32::from(lib_ordinal) | ((symbol_offsets[i] as u32) << 9),
        );
    }

    // Pad a couple of bytes (related to the MAX_SEGMENT_COUNT).
    string_pool[str_offset..].fill(0);

    Ok(())
}

fn write_uuid(layout: &MachOLayout, sized_output: &mut SizedOutput<impl OutputFileData>) -> Result {
    verbose_timing_phase!("Write UUID");

    let hash = blake3::Hasher::new()
        .update_rayon(&sized_output.out)
        .finalize();

    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    let load_commands = section_buffers.get_mut(output_section_id::LOAD_COMMANDS);

    while !load_commands.is_empty() {
        let header = object::from_bytes::<LoadCommand<Endianness>>(load_commands)
            .map_err(|_| error!("Invalid load command header"))?
            .0;
        let cmd_type = header.cmd.get(LE);
        let cmd_size = header.cmdsize.get(LE) as usize;
        let mut cmd = load_commands
            .split_off_mut(..cmd_size)
            .context("Invalid load command allocation")?;

        if cmd_type == LC_UUID {
            let uuid_cmd = take_mut::<UuidCommand>(&mut cmd)?;
            let uuid_size = uuid_cmd.uuid.len();

            uuid_cmd.uuid.copy_from_slice(&hash.as_bytes()[..uuid_size]);
            // Match lld's UUID Version 3 from RFC 9562.
            uuid_cmd.uuid[6] = (uuid_cmd.uuid[6] & 0x0f) | 0x30;
            uuid_cmd.uuid[8] = (uuid_cmd.uuid[8] & 0x3f) | 0x80;
            return Ok(());
        }
    }

    bail!("Missing LC_UUID");
}

fn write_code_signature_metadata(
    layout: &MachOLayout,
    sized_output: &mut SizedOutput<impl OutputFileData>,
) -> Result {
    verbose_timing_phase!("Write code signature metadata");

    let code_signature_section = layout
        .section_layouts
        .get(output_section_id::CODE_SIGNATURE);
    let code_signature_identifier = code_signature_identifier(layout.args());
    let padded_identifier_size = code_signature_padded_identifier_size(layout.args()) as usize;

    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    let code_signature = section_buffers.get_mut(output_section_id::CODE_SIGNATURE);

    let encoder = CodeSignatureEncoder;
    let code_directory_size = encoder.code_directory_size(CS_SUPPORTSEXECSEG);
    ensure!(
        u64::from(code_directory_size) == CS_CODE_DIRECTORY_SIZE,
        "Unexpected code directory size"
    );

    let text_segment = layout
        .segment_layouts
        .segments
        .iter()
        .find(|segment| layout.program_segments.segment_def(segment.id).name == SegmentName::TEXT)
        .ok_or_else(|| error!("__TEXT segment is mandatory"))?;

    let code_directory = CodeDirectory {
        length: (code_signature_section.file_size - CS_BLOB_HEADERS_SIZE as usize) as u32,
        version: CS_SUPPORTSEXECSEG,
        flags: CS_ADHOC | CS_LINKER_SIGNED,
        hash_offset: code_directory_size + padded_identifier_size as u32,
        ident_offset: code_directory_size,
        n_special_slots: 0,
        n_code_slots: code_signature_section.file_offset.div_ceil(CS_BLOCK_SIZE) as u32,
        code_limit: code_signature_section.file_offset as u64,
        hash_size: CS_HASH_SIZE,
        hash_type: CS_HASHTYPE_SHA256,
        platform: 0,
        page_size: CS_BLOCK_SIZE_EXP,
        scatter_offset: 0,
        team_offset: 0,
        exec_seg_base: text_segment.sizes.file_offset as u64,
        exec_seg_limit: text_segment.sizes.file_size as u64,
        // TODO: change once shared libraries are supported
        exec_seg_flags: CS_EXECSEG_MAIN_BINARY,
    };

    let mut rest: &mut [u8] = code_signature;
    encoder.signature_super_blob(&mut rest, code_signature_section.file_size as u32, 1);
    encoder.blob_index(&mut rest, CSSLOT_CODEDIRECTORY, CS_BLOB_HEADERS_SIZE as u32);
    encoder.code_directory(&mut rest, &code_directory);

    let (identifier, hashes) = rest.split_at_mut(padded_identifier_size);
    identifier[..code_signature_identifier.len()].copy_from_slice(code_signature_identifier);
    identifier[code_signature_identifier.len()..].zero();
    hashes.zero();

    Ok(())
}

fn write_code_signature_hashes(
    layout: &MachOLayout,
    sized_output: &mut SizedOutput<impl OutputFileData>,
) -> Result {
    verbose_timing_phase!("Write code signature hashes");

    let code_signature_section = layout
        .section_layouts
        .get(output_section_id::CODE_SIGNATURE);
    let calculated_hashes: Vec<_> = sized_output.out[..code_signature_section.file_offset]
        .par_chunks(CS_BLOCK_SIZE)
        .map(Sha256::digest)
        .collect();
    let calculated_hashes = calculated_hashes.into_iter().flatten().collect_vec();

    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    let code_signature = section_buffers.get_mut(output_section_id::CODE_SIGNATURE);
    let hashes_offset =
        (CS_HEADERS_SIZE + code_signature_padded_identifier_size(layout.args())) as usize;
    let hashes = code_signature
        .get_mut(hashes_offset..)
        .ok_or_else(|| error!("Invalid CODE_SIGNATURE allocation"))?;

    hashes.copy_from_slice(&calculated_hashes);

    // Match lld's workaround for the macOS kernel caching signature-verification
    // data before the final code signature has been written:
    //
    // https://openradar.appspot.com/FB8914231
    sized_output
        .out
        .invalidate(code_signature_section.file_offset + code_signature_section.file_size);

    Ok(())
}

struct MachOSymbolTableWriter {
    next_strtab_offset: u32,
}

impl MachOSymbolTableWriter {
    fn write_str(&mut self, name: &[u8], buffers: &mut OutputSectionPartMap<&mut [u8]>) -> u32 {
        let len_with_terminator = name.len() + 1;
        let offset = self.next_strtab_offset;
        let out = buffers
            .get_mut(part_id::STRTAB)
            .split_off_mut(..len_with_terminator)
            .unwrap();
        out[..name.len()].copy_from_slice(name);
        out[name.len()] = 0;
        self.next_strtab_offset += len_with_terminator as u32;
        offset
    }

    #[inline(always)]
    fn define_symbol(
        &mut self,
        buffers: &mut OutputSectionPartMap<&mut [u8]>,
        name: &[u8],
        section: u8,
        symbol_type: object::macho::SymbolFlags,
        desc: object::macho::SymbolDesc,
        value: u64,
    ) -> Result {
        let entry = self.write_entry(name, buffers)?;
        entry.n_sect = section;
        entry.n_type = symbol_type;
        entry.n_value.set(LE, value);
        entry.n_desc.set(LE, desc);

        Ok(())
    }

    fn write_entry<'out>(
        &mut self,
        name: &[u8],
        buffers: &'out mut OutputSectionPartMap<&mut [u8]>,
    ) -> Result<&'out mut SymtabEntry> {
        let string_offset = self.write_str(name, buffers);
        let entry_bytes = buffers
            .get_mut(part_id::SYMTAB_GLOBAL)
            .split_off_mut(..size_of::<SymtabEntry>())
            .unwrap();
        let entry: &mut SymtabEntry = from_bytes_mut(entry_bytes)
            .map_err(|_| error!("Invalid SYMTAB_GLOBAL entry allocation"))?
            .0;
        entry.n_strx.set(LE, string_offset);
        Ok(entry)
    }
}

fn write_symbols<'data>(
    object: &ObjectLayout<'data, MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    symbol_writer: &mut MachOSymbolTableWriter,
) -> Result {
    for ((sym_index, sym), flags) in object
        .object
        .enumerate_symbols()
        .zip(layout.per_symbol_flags.raw_range(object.symbol_id_range))
    {
        let symbol_id = object.symbol_id_range.input_to_id(sym_index);
        let Some(info) = SymbolCopyInfo::new(
            object.object,
            sym_index,
            sym,
            symbol_id,
            &layout.symbol_db,
            flags.get(),
            &object.sections,
        ) else {
            continue;
        };

        let mut value = 0;
        let (section, symbol_type, desc) = if let Some(section_index) =
            object.object.symbol_section(sym, sym_index)?
        {
            let section_id = match &object.sections[section_index.0] {
                SectionSlot::Loaded(_) => object
                    .section_part_id(section_index, &layout.symbol_db.section_part_ids)
                    .output_section_id::<MachO>(),
                _ => bail!(
                    "Tried to copy a symbol in a section we didn't load. {}",
                    layout.symbol_debug(symbol_id)
                ),
            };
            let primary_id = layout.output_sections.primary_output_section(section_id);
            let n_type = sym.n_type.with_type(N_SECT);
            let n_sect = macho_section_index(layout, primary_id).with_context(|| {
                format!(
                    "No Mach-O section index for {} while writing {}",
                    primary_id,
                    layout.symbol_debug(symbol_id)
                )
            })?;
            let n_desc = sym.n_desc.get(LE);
            (n_sect, n_type, n_desc)
        } else if sym.is_absolute() {
            let n_desc = sym.n_desc.get(LE);
            (0, sym.n_type.with_type(N_ABS), n_desc)
        } else if sym.as_common().is_some() {
            let n_sect = macho_section_index(layout, crate::macho::output_section_id::BSS)
                .context("No Mach-O section index for common symbols")?;
            (n_sect, sym.n_type.with_type(N_SECT), Default::default())
        } else if sym.is_undefined() {
            let n_desc = sym.n_desc.get(LE);
            (0, sym.n_type.with_type(N_UNDF), n_desc)
        } else {
            bail!(
                "Attempted to output Mach-O symbol {} with unexpected n_type {:#x}, n_sect {}, n_value {:#x}",
                String::from_utf8_lossy(info.name),
                sym.n_type,
                sym.n_sect,
                sym.n_value.get(LE),
            )
        };

        if let Some(res) = layout.local_symbol_resolution(symbol_id) {
            value = res.value_for_symbol_table();
        }

        symbol_writer.define_symbol(buffers, info.name, section, symbol_type, desc, value)?;
    }

    Ok(())
}

// TODO: This is inefficient; simplify it once load commands use a table allocator instead of
// being modeled as a section.
fn macho_section_index(layout: &MachOLayout<'_>, section_id: OutputSectionId) -> Result<u8> {
    // The section index is one-based.
    let mut section_idx = 1u8;
    for event in &layout.output_order {
        match event {
            OrderEvent::Section(current)
                if layout.output_sections.will_emit_section(current)
                    && layout
                        .output_sections
                        .identity(current)
                        .is_some_and(|identity| identity.format_specific().is_some()) =>
            {
                if current == section_id {
                    return Ok(section_idx);
                }
                section_idx = section_idx
                    .checked_add(1)
                    .ok_or(error!("Section index out of range (u8)"))?;
            }
            _ => {}
        }
    }

    bail!("cannot find the output section")
}
