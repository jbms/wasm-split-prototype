use std::collections::{HashMap, HashSet, VecDeque};

use crate::dep_graph::{DepGraph, DepNode};
use crate::read::{ExportId, ImportId, InputFuncId, InputModule, SymbolIndex};
use anyhow::{anyhow, bail};
use lazy_static::lazy_static;
use regex::Regex;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct SplitModule {
    pub module_name: String,
    pub load_func: SymbolIndex,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct SplitPoint {
    pub module_name: String,
    pub import: ImportId,
    pub import_func: InputFuncId,
    pub export: ExportId,
    pub export_func: InputFuncId,
}

pub fn get_split_modules(module: &InputModule) -> HashMap<String, SplitModule> {
    const PREFIX: &str = "__wasm_split_load_";
    let mut split_modules: HashMap<String, SplitModule> = HashMap::new();
    for (i, info) in module.symbols.iter().enumerate() {
        let wasmparser::SymbolInfo::Func {
            name: Some(symbol_name),
            ..
        } = info
        else {
            continue;
        };
        if !symbol_name.starts_with(PREFIX) {
            continue;
        }
        let name = &symbol_name[PREFIX.len()..];
        split_modules.insert(
            name.into(),
            SplitModule {
                module_name: String::from(name),
                load_func: i,
            },
        );
    }
    split_modules
}

pub fn get_split_points(module: &InputModule) -> anyhow::Result<Vec<SplitPoint>> {
    macro_rules! process_imports_or_exports {
        ($pattern:expr, $map:ident, $member:ident, $id_ty:ty) => {
            let mut $map = HashMap::<(String, String), $id_ty>::new();
            {
                lazy_static! {
                    static ref PATTERN: Regex = Regex::new($pattern).unwrap();
                }

                for (id, item) in module.$member.iter().enumerate() {
                    let Some(captures) = PATTERN.captures(&item.name) else {
                        continue;
                    };
                    let (_, [module_name, unique_id]) = captures.extract();
                    $map.insert((module_name.into(), unique_id.into()), id);
                }
            }
        };
    }

    process_imports_or_exports!(
        "__wasm_split_00(.*)00_import_([0-9a-f]{32})",
        import_map,
        imports,
        ImportId
    );
    process_imports_or_exports!(
        "__wasm_split_00(.*)00_export_([0-9a-f]{32})",
        export_map,
        exports,
        ExportId
    );

    let split_points = import_map
        .drain()
        .map(|(key, import_id)| -> anyhow::Result<SplitPoint> {
            let export_id = export_map.remove(&key).ok_or_else(|| {
                anyhow::anyhow!("No corresponding export for split import {key:?}")
            })?;
            let export = module.exports[export_id];
            let wasmparser::Export {
                kind: wasmparser::ExternalKind::Func,
                index,
                ..
            } = export
            else {
                bail!("Expected exported function but received: {export:?}");
            };
            let &import_func = module.imported_func_map.get(&import_id).ok_or_else(|| {
                anyhow!(
                    "Expected imported function but received: {:?}",
                    &module.imports[import_id]
                )
            })?;
            Ok(SplitPoint {
                module_name: key.0,
                import: import_id,
                import_func,
                export: export_id,
                export_func: index as InputFuncId,
            })
        })
        .collect::<anyhow::Result<Vec<SplitPoint>>>()?;

    for (key, _) in export_map.iter() {
        anyhow::bail!("No corresponding import for split export {key:?}");
    }

    Ok(split_points)
}

#[derive(Debug, Default)]
pub struct ReachabilityGraph {
    pub reachable: HashSet<DepNode>,
    pub parents: HashMap<DepNode, DepNode>,
}

#[derive(Debug, Default)]
pub struct OutputModuleInfo {
    pub included_symbols: HashSet<DepNode>,
    pub parents: HashMap<DepNode, DepNode>,
    pub shared_imports: HashSet<InputFuncId>,
    pub split_points: Vec<SplitPoint>,
}

impl OutputModuleInfo {
    pub fn print(&self, module_name: &str, module: &InputModule) {
        print_deps(module_name, module, &self.included_symbols, &self.parents);
    }
}

impl From<ReachabilityGraph> for OutputModuleInfo {
    fn from(reachability: ReachabilityGraph) -> Self {
        Self {
            included_symbols: reachability.reachable,
            parents: reachability.parents,
            ..Default::default()
        }
    }
}

fn print_deps(
    module_name: &str,
    module: &InputModule,
    reachable: &HashSet<DepNode>,
    parents: &HashMap<DepNode, DepNode>,
) {
    let format_dep = |dep: &DepNode| match dep {
        DepNode::Function(index) => {
            let name = module.names.functions.get(index);
            format!("func[{index}] <{name:?}>")
        }
        DepNode::DataSymbol(index) => {
            let symbol = module.symbols[*index];
            format!("{symbol:?}")
        }
    };

    println!("SPLIT: ============== {module_name}");
    let mut total_size: usize = 0;
    for dep in reachable.iter() {
        let DepNode::Function(index) = dep else {
            continue;
        };
        let size = index
            .checked_sub(module.imported_funcs.len())
            .map(|defined_index| {
                module.defined_funcs[index - module.imported_funcs.len()]
                    .body
                    .range()
                    .len()
            })
            .unwrap_or_default();
        total_size += size;
        println!("   {} size={size:?}", format_dep(dep));
        let mut node = dep;
        while let Some(parent) = parents.get(node) {
            println!("      <== {}", format_dep(parent));
            node = parent;
        }
    }
    println!("SPLIT: ============== {module_name}  : total size: {total_size}");
}

impl ReachabilityGraph {
    pub fn print(&self, module_name: &str, module: &InputModule) {
        print_deps(module_name, module, &self.reachable, &self.parents);
    }
}

pub fn find_reachable_deps(
    deps: &DepGraph,
    roots: &HashSet<DepNode>,
    exclude: &HashSet<DepNode>,
) -> ReachabilityGraph {
    let mut queue: VecDeque<DepNode> = roots.iter().copied().collect();
    let mut seen = HashSet::<DepNode>::new();
    let mut parents = HashMap::<DepNode, DepNode>::new();
    while let Some(node) = queue.pop_front() {
        seen.insert(node);
        let Some(children) = deps.get(&node) else {
            continue;
        };
        for child in children {
            if seen.contains(&child) || exclude.contains(&child) {
                continue;
            }
            parents.entry(*child).or_insert(node);
            queue.push_back(*child);
        }
    }
    ReachabilityGraph {
        reachable: seen,
        parents,
    }
}

pub fn get_main_module_roots(
    module: &InputModule,
    split_points: &[SplitPoint],
) -> HashSet<DepNode> {
    let mut roots: HashSet<DepNode> = HashSet::new();
    if let Some(id) = module.start {
        roots.insert(DepNode::Function(id));
    }
    for export in module.exports.iter() {
        let wasmparser::Export {
            index,
            kind: wasmparser::ExternalKind::Func,
            ..
        } = export
        else {
            continue;
        };
        roots.insert(DepNode::Function(*index as usize));
    }
    for func_id in 0..module.imported_funcs.len() {
        roots.insert(DepNode::Function(func_id));
    }
    for split_point in split_points.iter() {
        roots.remove(&DepNode::Function(split_point.export_func));
        roots.remove(&DepNode::Function(split_point.import_func));
    }
    roots
}

pub fn get_split_points_by_module(
    split_points: &[SplitPoint],
) -> HashMap<String, Vec<&SplitPoint>> {
    split_points
        .iter()
        .fold(HashMap::new(), |mut map, split_point| {
            map.entry(split_point.module_name.clone())
                .or_insert_with(|| Vec::new())
                .push(&split_point);
            map
        })
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub enum SplitModuleIdentifier {
    Main,
    Split(String),
    Chunk(Vec<String>),
}

impl SplitModuleIdentifier {
    pub fn name(&self) -> String {
        match self {
            Self::Main => "main".to_string(),
            Self::Split(name) => name.clone(),
            Self::Chunk(names) => names.join("_"),
        }
    }
}

#[derive(Debug, Default)]
pub struct SplitProgramInfo {
    pub output_modules: Vec<(SplitModuleIdentifier, OutputModuleInfo)>,
    pub output_module_identifiers: HashMap<SplitModuleIdentifier, usize>,
    pub shared_funcs: HashSet<InputFuncId>,
    pub symbol_output_module: HashMap<DepNode, usize>,
}

pub fn compute_split_modules(
    module: &InputModule,
    dep_graph: &DepGraph,
    split_points: &[SplitPoint],
) -> anyhow::Result<SplitProgramInfo> {
    let split_points_by_module = get_split_points_by_module(&split_points[..]);

    println!("split_points={split_points:?}");

    let split_func_map: HashMap<InputFuncId, InputFuncId> = split_points
        .iter()
        .map(|split_point| (split_point.import_func, split_point.export_func))
        .collect();

    let remove_ignored_deps = |deps: &mut HashSet<DepNode>| {
        for split_point in split_points.iter() {
            deps.remove(&DepNode::Function(split_point.import_func));
        }
    };
    let remove_ignored_funcs = |deps: &mut HashSet<InputFuncId>| {
        for split_point in split_points.iter() {
            deps.remove(&split_point.import_func);
        }
    };

    let main_roots = get_main_module_roots(module, &split_points);

    let mut main_deps = find_reachable_deps(dep_graph, &main_roots, &HashSet::new());

    remove_ignored_deps(&mut main_deps.reachable);

    // Determine reachable symbols (excluding main module symbols) for each
    // split module. Symbols may be reachable from more than one split module;
    // these symbols will be moved to a separate module.
    let mut split_module_candidates: HashMap<String, ReachabilityGraph> = split_points_by_module
        .iter()
        .map(|(module_name, entry_points)| {
            let mut roots = HashSet::<DepNode>::new();
            for entry_point in entry_points.iter() {
                roots.insert(DepNode::Function(entry_point.export_func));
            }
            let mut split_functions = find_reachable_deps(dep_graph, &roots, &main_deps.reachable);
            remove_ignored_deps(&mut split_functions.reachable);
            (module_name.clone(), split_functions)
        })
        .collect();

    // Set of split modules from which each symbol is reachable.
    let mut dep_candidate_modules = HashMap::<DepNode, Vec<String>>::new();
    for (module_name, deps) in split_module_candidates.iter() {
        for dep in deps.reachable.iter() {
            dep_candidate_modules
                .entry(*dep)
                .or_default()
                .push(module_name.clone());
        }
    }

    let mut program_info = SplitProgramInfo::default();

    let mut split_module_contents = HashMap::<SplitModuleIdentifier, OutputModuleInfo>::new();

    split_module_contents.insert(SplitModuleIdentifier::Main, main_deps.into());

    for (dep, mut modules) in dep_candidate_modules {
        if modules.len() > 1 {
            modules.sort();
            for module in modules.iter() {
                let module_contents = split_module_candidates.get_mut(module).unwrap();
                module_contents.reachable.remove(&dep);
            }
            split_module_contents
                .entry(SplitModuleIdentifier::Chunk(modules))
                .or_default()
                .included_symbols
                .insert(dep);
        }
    }

    split_module_contents.extend(
        split_module_candidates
            .drain()
            .map(|(module_name, deps)| (SplitModuleIdentifier::Split(module_name), deps.into())),
    );

    for contents in split_module_contents.values_mut() {
        for symbol in contents.included_symbols.iter() {
            let Some(neighbors) = dep_graph.get(symbol) else {
                continue;
            };
            for mut called_func_id in neighbors.iter().filter_map(|symbol| match symbol {
                DepNode::Function(func_id) => Some(*func_id),
                _ => None,
            }) {
                called_func_id = *split_func_map
                    .get(&called_func_id)
                    .unwrap_or(&called_func_id);
                if !contents
                    .included_symbols
                    .contains(&DepNode::Function(called_func_id))
                {
                    contents.shared_imports.insert(called_func_id);
                    program_info.shared_funcs.insert(called_func_id);
                }
            }
        }
        remove_ignored_funcs(&mut contents.shared_imports);
    }
    remove_ignored_funcs(&mut program_info.shared_funcs);

    for split_point in split_points {
        program_info.shared_funcs.insert(split_point.export_func);
        let output_module = split_module_contents
            .get_mut(&SplitModuleIdentifier::Split(
                split_point.module_name.to_string(),
            ))
            .unwrap();
        output_module.split_points.push(split_point.clone());
    }

    program_info.output_modules = split_module_contents.drain().collect();
    program_info
        .output_modules
        .sort_by_key(|(identifier, _)| (*identifier).clone());
    program_info.output_module_identifiers = program_info
        .output_modules
        .iter()
        .enumerate()
        .map(|(index, (identifier, _))| (identifier.clone(), index))
        .collect();

    for (output_index, (_, info)) in program_info.output_modules.iter().enumerate() {
        for &symbol in info.included_symbols.iter() {
            program_info
                .symbol_output_module
                .insert(symbol, output_index);
        }
    }

    Ok(program_info)
}
