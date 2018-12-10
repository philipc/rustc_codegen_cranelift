extern crate gimli;

use crate::prelude::*;

use gimli::write::{
    Address, AttributeValue, DebugAbbrev, DebugInfo, DebugLine, DebugStr, EndianVec, Result, SectionId,
    StringTable, UnitEntryId, UnitId, UnitTable, Writer, CompilationUnit,
    LineProgramTable, LineProgram, LineProgramId,
};
use gimli::Format;

// FIXME: use target endian
use gimli::NativeEndian;

pub struct DebugContext {
    line_programs: LineProgramTable,
    line_program_id: LineProgramId,
    strings: StringTable,
    units: UnitTable,
    unit_id: UnitId,
    symbols: Vec<ExternalName>,
    debug_abbrev_id: DebugSectionId,
    debug_info_id: DebugSectionId,
    debug_line_id: DebugSectionId,
    debug_str_id: DebugSectionId,
}

impl DebugContext {
    pub fn new(tcx: TyCtxt, address_size: u8, module: &mut Module<impl Backend + 'static>) -> Self {
        let mut units = UnitTable::default();
        let mut strings = StringTable::default();
        let mut line_programs = LineProgramTable::default();
        // TODO: this should be configurable
        let version = 4;
        let unit_id = units.add(CompilationUnit::new(version, address_size, Format::Dwarf32));
        let line_program_id;
        {
            // FIXME: how to get version when building out of tree?
            // Normally this would use option_env!("CFG_VERSION").
            let producer = format!("cranelift (rustc version {})", "unknown version");
            let name = match tcx.sess.local_crate_source_file {
                Some(ref path) => path.to_string_lossy().into_owned().into_bytes(),
                None => tcx.crate_name(LOCAL_CRATE).as_str().as_bytes().to_vec(),
            };
            let comp_dir = tcx.sess.working_dir.0.to_string_lossy().as_bytes().to_vec();

            line_program_id = line_programs.add(LineProgram::new(
                version,
                address_size,
                Format::Dwarf32,
                // FIXME: get constants from somewhere
                1, 1, -5, 14,
                &comp_dir,
                &name,
                // FIXME (FileInfo)
                None,
            ));

            let unit = units.get_mut(unit_id);
            let root = unit.root();
            let root = unit.get_mut(root);
            root.set(
                gimli::DW_AT_producer,
                AttributeValue::StringRef(strings.add(producer)),
            );
            root.set(
                gimli::DW_AT_language,
                AttributeValue::Language(gimli::DW_LANG_Rust),
            );
            root.set(gimli::DW_AT_name, AttributeValue::StringRef(strings.add(name)));
            root.set(gimli::DW_AT_comp_dir, AttributeValue::StringRef(strings.add(comp_dir)));
            root.set(gimli::DW_AT_stmt_list, AttributeValue::LineProgramRef(line_program_id));
            // FIXME: DW_AT_low_pc
            // FIXME: DW_AT_ranges
        }

        let debug_abbrev_id = module.declare_debug_section(SectionId::DebugAbbrev.name()).unwrap();
        let debug_info_id = module.declare_debug_section(SectionId::DebugInfo.name()).unwrap();
        let debug_line_id = module.declare_debug_section(SectionId::DebugLine.name()).unwrap();
        let debug_str_id = module.declare_debug_section(SectionId::DebugStr.name()).unwrap();

        DebugContext {
            line_programs,
            line_program_id,
            strings,
            units,
            unit_id,
            symbols: Vec::new(),
            debug_abbrev_id,
            debug_info_id,
            debug_line_id,
            debug_str_id,
        }
    }

    pub fn emit(&self, module: &mut Module<impl Backend + 'static>) {
        let mut debug_abbrev = DebugAbbrev::from(WriterRelocate::new(self));
        let mut debug_info = DebugInfo::from(WriterRelocate::new(self));
        let mut debug_line = DebugLine::from(WriterRelocate::new(self));
        let mut debug_str = DebugStr::from(WriterRelocate::new(self));

        let debug_line_offsets = self.line_programs.write(&mut debug_line).unwrap();
        let debug_str_offsets = self.strings.write(&mut debug_str).unwrap();
        self.units
            .write(&mut debug_abbrev, &mut debug_info, &debug_line_offsets, &debug_str_offsets)
            .unwrap();

        module
            .define_debug_section(
                self.debug_abbrev_id,
                DebugSectionContext {
                    data: debug_abbrev.0.writer.into_vec(),
                    relocs: debug_abbrev.0.relocs,
                },
            )
            .unwrap();
        module
            .define_debug_section(
                self.debug_line_id,
                DebugSectionContext {
                    data: debug_line.0.writer.into_vec(),
                    relocs: debug_line.0.relocs,
                },
            )
            .unwrap();
        module
            .define_debug_section(
                self.debug_info_id,
                DebugSectionContext {
                    data: debug_info.0.writer.into_vec(),
                    relocs: debug_info.0.relocs,
                },
            )
            .unwrap();
        module
            .define_debug_section(
                self.debug_str_id,
                DebugSectionContext {
                    data: debug_str.0.writer.into_vec(),
                    relocs: debug_str.0.relocs,
                },
            )
            .unwrap();
    }

    fn section_name(&self, id: SectionId) -> ExternalName {
        let debugid = match id {
            SectionId::DebugAbbrev => self.debug_abbrev_id,
            SectionId::DebugInfo => self.debug_info_id,
            SectionId::DebugLine => self.debug_line_id,
            SectionId::DebugStr => self.debug_str_id,
            _ => unimplemented!(),
        };
        FuncOrDataId::DebugSection(debugid).into()
    }
}

pub struct FunctionDebugContext<'a> {
    debug_context: &'a mut DebugContext,
    entry_id: UnitEntryId,
    span: Span,
    address: Address,
}

impl<'a> FunctionDebugContext<'a> {
    pub fn new(
        tcx: TyCtxt,
        debug_context: &'a mut DebugContext,
        mir: &Mir,
        func_id: FuncId,
        name: &str,
        _sig: &Signature,
    ) -> Self {
        let unit = debug_context.units.get_mut(debug_context.unit_id);
        // FIXME: add to appropriate scope intead of root
        let scope = unit.root();
        let entry_id = unit.add(scope, gimli::DW_TAG_subprogram);
        let entry = unit.get_mut(entry_id);
        let name_id = debug_context.strings.add(name);
        let symbol = debug_context.symbols.len();
        debug_context.symbols.push(FuncOrDataId::Func(func_id).into());
        let address = Address::Relative { symbol, addend: 0};
        let span = mir.span;
        let loc = tcx.sess.source_map().lookup_char_pos(span.lo());
        // FIXME: use file index into unit's line table
        // FIXME: specify directory too?
        let line_program = debug_context.line_programs.get_mut(debug_context.line_program_id);
        let file_id = line_program.add_file(loc.file.name.to_string().as_bytes(), line_program.default_directory(), None);
        entry.set(gimli::DW_AT_linkage_name, AttributeValue::StringRef(name_id));
        entry.set(gimli::DW_AT_decl_file, AttributeValue::FileIndex(file_id));
        entry.set(gimli::DW_AT_decl_line, AttributeValue::Udata(loc.line as u64));
        // FIXME: probably omit this
        entry.set(gimli::DW_AT_decl_column, AttributeValue::Udata(loc.col.to_usize() as u64));
        entry.set(gimli::DW_AT_low_pc, AttributeValue::Address(address));
        FunctionDebugContext {
            debug_context,
            entry_id,
            span,
            address,
        }
    }

    pub fn define(
        &mut self,
        tcx: TyCtxt,
        module: &mut Module<impl Backend>,
        context: &Context,
        spans: &[Span],
    ) {
        let set_loc = |line_program: &mut LineProgram, span: Span| {
            let loc = tcx.sess.source_map().lookup_char_pos(span.lo());
            // FIXME: directory
            let file = loc.file.name.to_string();
            let file_id = line_program.add_file(file.as_bytes(), line_program.default_directory(), None);
            line_program.row().file = file_id;
            line_program.row().line = loc.line as u64;
            line_program.row().column = loc.col.to_usize() as u64;
        };

        let func = &context.func;
        let encinfo = module.isa().encoding_info();

        // FIXME: this is probably all wildly inaccurate
        // Have a look at LLVM's DwarfDebug::beginInstruction() for inspiration.
        let line_program = self.debug_context.line_programs.get_mut(self.debug_context.line_program_id);
        line_program.begin_sequence(Some(self.address));
        let mut prev_span = None;
        let mut first = true;
        let mut end = 0;
        for ebb in func.layout.ebbs() {
            for (offset, inst, size) in func.inst_offsets(ebb, &encinfo) {
                let srcloc = func.srclocs[inst];
                if srcloc.is_default() {
                    prev_span = None;
                    continue;
                }
                let span = spans[srcloc.bits() as usize];
                if prev_span == Some(span) {
                    continue;
                }
                prev_span = Some(span);

                if first {
                    if offset != 0 {
                        set_loc(line_program, self.span);
                        line_program.generate_row();
                    }
                    // FIXME: is this correct?
                    line_program.row().prologue_end = true;
                    first = false;
                }

                // FIXME: set row.is_statement
                line_program.row().address_offset = offset as u64;
                set_loc(line_program, span);
                line_program.generate_row();
                end = offset + size;
            }
        }
        if func.code_size != end {
            line_program.row().address_offset = end as u64;
            // FIXME: probably shouldn't use this span here
            set_loc(line_program, self.span);
            line_program.generate_row();
        }
        line_program.end_sequence(func.code_size as u64);
    }
}

struct WriterRelocate<'a> {
    ctx: &'a DebugContext,
    relocs: Vec<DebugReloc>,
    writer: EndianVec<NativeEndian>,
}

impl<'a> WriterRelocate<'a> {
    fn new(ctx: &'a DebugContext) -> Self {
        WriterRelocate {
            ctx,
            relocs: Vec::new(),
            writer: EndianVec::new(NativeEndian),
        }
    }
}

impl<'a> Writer for WriterRelocate<'a> {
    type Endian = NativeEndian;

    fn endian(&self) -> Self::Endian {
        self.writer.endian()
    }

    fn len(&self) -> usize {
        self.writer.len()
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write(bytes)
    }

    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.writer.write_at(offset, bytes)
    }

    fn write_address(&mut self, address: Address, size: u8) -> Result<()> {
        match address {
            Address::Absolute(val) => self.write_word(val, size),
            Address::Relative { symbol, addend } => {
                let offset = self.len() as u32;
                self.relocs.push(DebugReloc {
                    offset,
                    size,
                    name: self.ctx.symbols[symbol].clone(),
                    addend,
                });
                self.write_word(0, size)
            }
        }
    }

    fn write_offset(&mut self, val: usize, section: SectionId, size: u8) -> Result<()> {
        let offset = self.len() as u32;
        let name = self.ctx.section_name(section);
        self.relocs.push(DebugReloc {
            offset,
            size,
            name,
            addend: val as i64,
        });
        self.write_word(0, size)
    }

    fn write_offset_at(
        &mut self,
        offset: usize,
        val: usize,
        section: SectionId,
        size: u8,
    ) -> Result<()> {
        let name = self.ctx.section_name(section);
        self.relocs.push(DebugReloc {
            offset: offset as u32,
            size,
            name,
            addend: val as i64,
        });
        self.write_word_at(offset, 0, size)
    }
}
