use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    ops::{Deref, Range},
};

use anyhow::{bail, Context};
use walrus::Module;

pub type SymbolIndex = usize;

pub struct SymbolTable<'a>(Vec<wasmparser::SymbolInfo<'a>>);

impl<'a> SymbolTable<'a> {
    fn get_symbol_dep_node(&self, symbol_index: usize) -> Option<DepNode> {
        match self.0[symbol_index] {
            wasmparser::SymbolInfo::Func { index, .. } => Some(DepNode::Function(index as usize)),
            wasmparser::SymbolInfo::Data { .. } => Some(DepNode::DataSymbol(symbol_index)),
            _ => None,
        }
    }
}

impl<'a> Deref for SymbolTable<'a> {
    type Target = Vec<wasmparser::SymbolInfo<'a>>;
    fn deref(&self) -> &<Self as Deref>::Target {
        &self.0
    }
}

impl SymbolRangeInfo {
    fn new(
        wasm: &[u8],
        module: &walrus::Module,
        symbol_table: &SymbolTable,
    ) -> anyhow::Result<Self> {
        let data_segment_ranges = get_data_segment_ranges(wasm)?;
        let mut defined_funcs: Vec<(walrus::FunctionId, Range<usize>)> = module
            .funcs
            .iter_local()
            .filter_map(|(id, func)| {
                func.original_range
                    .map(|range| (id, range.start as usize..range.end as usize))
            })
            .collect();
        defined_funcs.sort_by(|(_, a), (_, b)| a.start.cmp(&b.start));

        let mut data_symbols: Vec<(usize, wasmparser::DefinedDataSymbol)> = symbol_table
            .iter()
            .enumerate()
            .filter_map(|(i, info)| match info {
                wasmparser::SymbolInfo::Data {
                    symbol: Some(symbol),
                    ..
                } => Some((i, *symbol)),
                _ => None,
            })
            .collect();
        data_symbols.sort_by(|(_, a), (_, b)| (a.index, a.offset).cmp(&(b.index, b.offset)));
        Ok(SymbolRangeInfo {
            defined_funcs,
            data_segment_ranges,
            data_symbols,
        })
    }

    fn find_function_containing_range(&self, range: Range<usize>) -> anyhow::Result<usize> {
        let Some(func_index) = find_by_range(&self.defined_funcs, &range, |(_, func_range)| {
            func_range.clone()
        }) else {
            bail!("No match for function relocation range {range:?}");
        };
        Ok(self.defined_funcs[func_index].0.index())
    }

    fn find_data_segment_containing_range(&self, range: Range<usize>) -> anyhow::Result<usize> {
        let Some(index) = find_by_range(&self.data_segment_ranges, &range, |data_segment_range| {
            data_segment_range.clone()
        }) else {
            bail!("No match for data relocation range {range:?}");
        };
        Ok(index)
    }

    fn find_data_symbol_containing_range(&self, range: Range<usize>) -> anyhow::Result<usize> {
        let get_section_relative_range = |defined_symbol: &wasmparser::DefinedDataSymbol| {
            let offset = defined_symbol.offset as usize;
            let size = defined_symbol.size as usize;
            let base = self.data_segment_ranges[defined_symbol.index as usize].start;
            (base + offset)..(base + offset + size)
        };
        let Some(index) = find_by_range(&self.data_symbols, &range, |(_, defined_symbol)| {
            get_section_relative_range(defined_symbol)
        }) else {
            bail!(
                "No match for data relocation range {range:?} {data_segment_ranges:?}",
                data_segment_ranges = &self.data_segment_ranges
            );
        };
        Ok(self.data_symbols[index].0)
    }
}

pub struct ModuleInfo<'a> {
    pub module: &'a walrus::Module,
    pub func_id_map: HashMap<usize, &'a walrus::Function>,
    pub symbol_table: SymbolTable<'a>,
    pub func_symbols: HashMap<usize, SymbolIndex>,
    pub dep_graph: DepGraph,
}

pub type DataSegmentRange = Range<usize>;

fn get_symbol_table<'a>(module: &'a walrus::Module) -> anyhow::Result<SymbolTable<'a>> {
    let (Some(section), None) = ({
        let mut iter = module
            .customs
            .iter()
            .map(|(_id, section)| section)
            .filter(|section| section.name() == "linking");
        (iter.next(), iter.next())
    }) else {
        bail!("No linking section found");
    };
    let raw_section = section
        .as_any()
        .downcast_ref::<walrus::RawCustomSection>()
        .unwrap();
    let reader = wasmparser::LinkingSectionReader::new(&raw_section.data[..], 0)?;
    let (Some(symbol_table), None) = ({
        let mut iter = reader
            .subsections()
            .filter_map(|subsection| match subsection {
                Ok(wasmparser::Linking::SymbolTable(map)) => Some(map),
                _ => None,
            });
        (iter.next(), iter.next())
    }) else {
        bail!("No symbol table found");
    };
    Ok(SymbolTable(
        symbol_table.into_iter().collect::<Result<Vec<_>, _>>()?,
    ))
}

const CODE_SECTION_ID: u32 = 10;

const DATA_SECTION_ID: u32 = 11;

fn get_data_segment_ranges(wasm: &[u8]) -> anyhow::Result<Vec<DataSegmentRange>> {
    let mut ranges: Vec<DataSegmentRange> = Vec::new();
    let parser = wasmparser::Parser::new(0);
    for payload in parser.parse_all(wasm) {
        match payload? {
            wasmparser::Payload::DataSection(data_section) => {
                let data_section_offset = data_section.range().start;
                for data_segment_result in data_section.into_iter() {
                    let wasmparser::Data { data, .. } = data_segment_result?;
                    let offset = data.as_ptr() as usize - wasm.as_ptr() as usize;
                    // We can't use `wasmparser::Data::range` because it
                    // includes the segment header, and we need the range of the
                    // segment data.)
                    ranges.push(
                        (offset - data_section_offset)..(offset - data_section_offset + data.len()),
                    );
                }
            }
            _ => {}
        }
    }
    Ok(ranges)
}

#[derive(Debug, PartialEq, Eq, Hash, Copy, PartialOrd, Ord, Clone)]
pub enum DepNode {
    Function(usize),
    DataSymbol(usize),
}

impl<'a> ModuleInfo<'a> {
    pub fn new(wasm: &[u8], module_opt: &'a mut Option<Module>) -> anyhow::Result<Self> {
        *module_opt = Some(Module::from_buffer(wasm)?);
        let module = &module_opt.as_ref().unwrap();
        let func_id_map = module
            .funcs
            .iter()
            .map(|f| (f.id().index() as usize, f))
            .collect();
        let symbol_table = get_symbol_table(module)?;
        let func_symbols: HashMap<usize, SymbolIndex> = symbol_table
            .iter()
            .enumerate()
            .filter_map(|(i, info)| match info {
                wasmparser::SymbolInfo::Func { index, .. } => Some((*index as usize, i)),
                _ => None,
            })
            .collect();
        let symbol_range_info = SymbolRangeInfo::new(wasm, module, &symbol_table)?;
        let dep_graph = get_dependencies(module, &symbol_range_info, &symbol_table)?;
        Ok(Self {
            module,
            func_id_map,
            symbol_table,
            func_symbols,
            dep_graph,
        })
    }
}

pub type DepGraph = HashMap<DepNode, HashSet<DepNode>>;
