extern crate gimli;

use crate::prelude::*;

use gimli::write::{
    Address, AttributeValue, DebugAbbrev, DebugInfo, DebugStr, EndianVec, Result, SectionId,
    StringTable, UnitId, UnitTable, Writer,
};
use gimli::Format;

// FIXME: use target endian
use gimli::NativeEndian;

pub struct DebugContext {
    strings: StringTable,
    units: UnitTable,
    unit_id: UnitId,
    debug_abbrev_id: DebugSectionId,
    debug_info_id: DebugSectionId,
    debug_str_id: DebugSectionId,
}

impl DebugContext {
    pub fn new(tcx: TyCtxt, address_size: u8, module: &mut Module<impl Backend + 'static>) -> Self {
        let mut units = UnitTable::default();
        let mut strings = StringTable::default();
        // TODO: this should be configurable
        let version = 5;
        let unit_id = units.add(version, address_size, Format::Dwarf32);
        {
            // FIXME: how to get version when building out of tree?
            // Normally this would use option_env!("CFG_VERSION").
            let producer = format!("cranelift (rustc version {})", "unknown version");
            let name = match tcx.sess.local_crate_source_file {
                Some(ref path) => strings.add(&*path.to_string_lossy()),
                None => strings.add(&*tcx.crate_name(LOCAL_CRATE).as_str()),
            };
            let comp_dir = strings.add(&*tcx.sess.working_dir.0.to_string_lossy());

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
            root.set(gimli::DW_AT_name, AttributeValue::StringRef(name));
            root.set(gimli::DW_AT_comp_dir, AttributeValue::StringRef(comp_dir));
            // FIXME: DW_AT_stmt_list
            // FIXME: DW_AT_low_pc
            // FIXME: DW_AT_ranges
        }

        let debug_abbrev_id = module.declare_debug_section(SectionId::DebugAbbrev.name()).unwrap();
        tcx.sess
            .warn(&format!("debug_abbrev_id {:?}", debug_abbrev_id));
        let debug_info_id = module.declare_debug_section(SectionId::DebugInfo.name()).unwrap();
        let debug_str_id = module.declare_debug_section(SectionId::DebugStr.name()).unwrap();

        DebugContext {
            strings,
            units,
            unit_id,
            debug_abbrev_id,
            debug_info_id,
            debug_str_id,
        }
    }

    pub fn emit(&self, module: &mut Module<impl Backend + 'static>) {
        let mut debug_abbrev = DebugAbbrev::from(WriterRelocate::new(self));
        let mut debug_info = DebugInfo::from(WriterRelocate::new(self));
        let mut debug_str = DebugStr::from(WriterRelocate::new(self));

        let debug_str_offsets = self.strings.write(&mut debug_str).unwrap();
        self.units
            .write(&mut debug_info, &mut debug_abbrev, &debug_str_offsets)
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
            SectionId::DebugStr => self.debug_str_id,
            _ => unimplemented!(),
        };
        FuncOrDataId::DebugSection(debugid).into()
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
        /*
        match address {
            Address::Absolute(val) => self.write_word(val, size),
            Address::Relative { symbol, addend } => {
                let offset = self.len() as u64;
                self.relocs.push(Relocation::Symbol {
                    offset,
                    symbol,
                    addend: addend as i32,
                    size,
                });
                self.write_word(0, size)
            }
        }
        */
        unimplemented!();
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
