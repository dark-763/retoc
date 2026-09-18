//! Container-wide export bundle layout and package load order.
//!
//! For container header versions up to `Initial` the cooker decided both of these in a
//! single pass over every package of the container at once, so neither can be derived
//! from one package on its own. This module reproduces that pass from the legacy
//! packages before any of them is converted, and hands the result to the per-package
//! conversion, which keeps taking its layout from the outside exactly as it did when
//! that layout came from `RETOC_BUNDLE_LAYOUT`.
//!
//! Written from a prose description of the 4.26 algorithm; no engine code was copied.
//! The steps are lettered to match the notes in TASK.md:
//!   a) packages sorted by `FPackageId`
//!   b) two nodes per export, Create before Serialize
//!   c) edges from the preload dependency table
//!   d) package order: reversed post-order DFS over bare package imports
//!   e) Kahn's algorithm with one ready-queue per package, drained package by package
//!   f) a bundle closes when the next node belongs to a different package

use crate::FPackageId;
use crate::legacy_asset::{FLegacyPackageHeader, get_package_object_full_name};
use crate::logging::Log;
use crate::zen::{EObjectFlags, FPackageIndex};
use crate::{info, verbose};
use anyhow::Result;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

const CREATE: u8 = 0;
const SERIALIZE: u8 = 1;

/// One package as the pass sees it: just enough of the legacy header to build the graph.
pub struct PassInput {
    pub package_id: FPackageId,
    pub package_name: String,
    pub header: FLegacyPackageHeader,
}

/// Layout and load order for every package of the container.
pub struct ContainerBundleLayout {
    layouts: HashMap<FPackageId, Vec<(u32, u32)>>,
    load_orders: HashMap<FPackageId, u32>,
}

impl ContainerBundleLayout {
    /// `(first_entry_index, entry_count)` per bundle, in bundle order. `None` means the
    /// pass never saw this package, which is an error at the call site rather than a
    /// reason to guess.
    pub fn layout_of(&self, package_id: FPackageId) -> Option<&Vec<(u32, u32)>> {
        self.layouts.get(&package_id)
    }
    pub fn load_order_of(&self, package_id: FPackageId) -> Option<u32> {
        self.load_orders.get(&package_id).copied()
    }
    pub fn contains(&self, package_id: FPackageId) -> bool {
        self.layouts.contains_key(&package_id)
    }
    pub fn len(&self) -> usize {
        self.layouts.len()
    }
    pub fn is_empty(&self) -> bool {
        self.layouts.is_empty()
    }
}

struct Package {
    id: FPackageId,
    header: FLegacyPackageHeader,
    node_base: usize,
    /// lowercase full object path -> local export index, for resolving another package's imports
    exports_by_path: HashMap<String, usize>,
    /// packages this one imports, as indices into the sorted package list
    imports: Vec<usize>,
    bundles: Vec<(u32, u32)>,
    first_bundle: Option<u32>,
}

pub fn compute_container_bundle_layout(input: Vec<PassInput>, log: &Log) -> Result<ContainerBundleLayout> {
    // a) packages in FPackageId order - this is the order the cooker's own package array
    // is left in, and the traversal below uses it for its roots
    let mut packages: Vec<Package> = input
        .into_iter()
        .map(|p| Package {
            id: p.package_id,
            header: p.header,
            node_base: 0,
            exports_by_path: HashMap::new(),
            imports: Vec::new(),
            bundles: Vec::new(),
            first_bundle: None,
        })
        .collect();
    packages.sort_by_key(|p| p.id.0);

    let mut index_by_name: HashMap<String, usize> = HashMap::with_capacity(packages.len());
    for (i, package) in packages.iter().enumerate() {
        index_by_name.insert(package.header.summary.package_name.to_ascii_lowercase(), i);
    }

    // b) node numbering: packages in order, exports by index, Create before Serialize
    let mut total_nodes = 0usize;
    for package in packages.iter_mut() {
        package.node_base = total_nodes;
        total_nodes += package.header.exports.len() * 2;
    }

    // Full object path of every export, so another package's import can be matched to it
    for package_index in 0..packages.len() {
        let export_count = packages[package_index].header.exports.len();
        let own_name = packages[package_index].header.summary.package_name.clone();
        for export_index in 0..export_count {
            let (_, full_path) = get_package_object_full_name(
                &packages[package_index].header,
                FPackageIndex::create_export(export_index as u32),
                '/',
                true,
                Some(&own_name),
            );
            packages[package_index].exports_by_path.insert(full_path, export_index);
        }
    }

    let mut out_edges: Vec<Vec<usize>> = vec![Vec::new(); total_nodes];
    let mut incoming: Vec<u32> = vec![0; total_nodes];
    let mut node_package: Vec<usize> = vec![0; total_nodes];
    let mut node_key: Vec<(u32, u8)> = vec![(0, 0); total_nodes];

    for (package_index, package) in packages.iter().enumerate() {
        for export_index in 0..package.header.exports.len() {
            let create = package.node_base + export_index * 2;
            let serialize = create + 1;
            node_package[create] = package_index;
            node_package[serialize] = package_index;
            node_key[create] = (export_index as u32, CREATE);
            node_key[serialize] = (export_index as u32, SERIALIZE);
            // b) an export cannot serialize before it is created
            out_edges[create].push(serialize);
            incoming[serialize] += 1;
        }
    }

    // c) edges from preload dependencies. An import only becomes an edge when it resolves
    // to a PUBLIC export of another package of this container: the cooker looks the
    // import up in a map built solely from public exports, and anything it does not find
    // there becomes an arc with no edge at all.
    let mut unresolved_imports = 0usize;
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (package_index, package) in packages.iter().enumerate() {
        for (export_index, export) in package.header.exports.iter().enumerate() {
            if export.first_export_dependency_index < 0 {
                continue;
            }
            let mut cursor = export.first_export_dependency_index as usize;
            let groups = [
                (export.serialize_before_serialize_dependencies, SERIALIZE, SERIALIZE),
                (export.create_before_serialize_dependencies, CREATE, SERIALIZE),
                (export.serialize_before_create_dependencies, SERIALIZE, CREATE),
                (export.create_before_create_dependencies, CREATE, CREATE),
            ];
            for (count, from_command, to_command) in groups {
                for offset in 0..count.max(0) as usize {
                    let dependency = package.header.preload_dependencies[cursor + offset];
                    let to_node = package.node_base + export_index * 2 + to_command as usize;

                    if dependency.is_export() {
                        let from_node = package.node_base + dependency.to_export_index() as usize * 2 + from_command as usize;
                        edges.push((from_node, to_node));
                    } else if dependency.is_import() {
                        let (owner_name, full_path) =
                            get_package_object_full_name(&package.header, dependency, '/', true, None);
                        if owner_name.starts_with("/Script") {
                            continue;
                        }
                        let Some(&target_index) = index_by_name.get(&owner_name.to_ascii_lowercase()) else {
                            unresolved_imports += 1;
                            continue;
                        };
                        // A package can carry an import of its own name; that is not an edge
                        // to another package, and treating it as one closes a cycle.
                        if target_index == package_index {
                            continue;
                        }
                        let Some(&target_export) = packages[target_index].exports_by_path.get(&full_path) else {
                            unresolved_imports += 1;
                            continue;
                        };
                        if (packages[target_index].header.exports[target_export].object_flags & (EObjectFlags::Public as u32)) == 0 {
                            continue;
                        }
                        let from_node = packages[target_index].node_base + target_export * 2 + from_command as usize;
                        edges.push((from_node, to_node));
                    }
                }
                cursor += count.max(0) as usize;
            }
        }
    }
    for (from_node, to_node) in edges {
        out_edges[from_node].push(to_node);
        incoming[to_node] += 1;
    }

    // d) package graph, built from bare package imports: an import with no outer whose
    // class is Package. The edge runs from the imported package to the importer, so the
    // neighbours of a package are the packages that import it.
    for package_index in 0..packages.len() {
        let mut targets: Vec<usize> = Vec::new();
        for import in &packages[package_index].header.imports {
            if !import.outer_index.is_null() {
                continue;
            }
            let class_name = packages[package_index].header.name_map.get(import.class_name)?.to_string();
            if class_name != "Package" {
                continue;
            }
            let name = packages[package_index].header.name_map.get(import.object_name)?.to_string();
            if name.starts_with("/Script") {
                continue;
            }
            if let Some(&target) = index_by_name.get(&name.to_ascii_lowercase())
                && target != package_index
            {
                targets.push(target);
            }
        }
        packages[package_index].imports = targets;
    }
    let mut importers_of: Vec<Vec<usize>> = vec![Vec::new(); packages.len()];
    for package_index in 0..packages.len() {
        for &target in &packages[package_index].imports {
            importers_of[target].push(package_index);
        }
    }
    for list in importers_of.iter_mut() {
        list.sort_by_key(|&i| packages[i].id.0);
        list.dedup();
    }
    let package_order = reversed_post_order(&importers_of);

    // e) and f)
    let mut ready: Vec<BinaryHeap<Reverse<(u32, u8, usize)>>> = (0..packages.len()).map(|_| BinaryHeap::new()).collect();
    for node in 0..total_nodes {
        if incoming[node] == 0 {
            let (export_index, command) = node_key[node];
            ready[node_package[node]].push(Reverse((export_index, command, node)));
        }
    }

    let mut bundle_counter: u32 = 0;
    let mut emitted = 0usize;
    let mut previous_package: Option<usize> = None;
    let mut entries_in_current_bundle: HashMap<usize, u32> = HashMap::new();
    let mut entry_cursor: HashMap<usize, u32> = HashMap::new();

    while emitted < total_nodes {
        let mut progressed = false;
        for &package_index in &package_order {
            while let Some(Reverse((_, _, node))) = ready[package_index].pop() {
                progressed = true;
                if previous_package != Some(package_index) {
                    let first_entry = *entry_cursor.get(&package_index).unwrap_or(&0);
                    packages[package_index].bundles.push((first_entry, 0));
                    if packages[package_index].first_bundle.is_none() {
                        packages[package_index].first_bundle = Some(bundle_counter);
                    }
                    bundle_counter += 1;
                    previous_package = Some(package_index);
                    entries_in_current_bundle.insert(package_index, 0);
                }
                let last = packages[package_index].bundles.len() - 1;
                packages[package_index].bundles[last].1 += 1;
                *entry_cursor.entry(package_index).or_insert(0) += 1;
                emitted += 1;

                for next in out_edges[node].clone() {
                    incoming[next] -= 1;
                    if incoming[next] == 0 {
                        let (export_index, command) = node_key[next];
                        ready[node_package[next]].push(Reverse((export_index, command, next)));
                    }
                }
            }
        }
        if !progressed {
            break;
        }
    }

    if emitted != total_nodes {
        anyhow::bail!(
            "Export dependency graph of the container has a cycle: {} of {} nodes could not be ordered. The bundle layout cannot be computed.",
            total_nodes - emitted,
            total_nodes
        );
    }

    verbose!(log, "Bundle layout pass: {} packages, {} nodes, {} bundles, {} imports left unresolved", packages.len(), total_nodes, bundle_counter, unresolved_imports);
    info!(log, "Computed export bundle layout for {} packages ({} bundles total)", packages.len(), bundle_counter);

    let mut layouts = HashMap::with_capacity(packages.len());
    let mut load_orders = HashMap::with_capacity(packages.len());
    for package in packages {
        load_orders.insert(package.id, package.first_bundle.unwrap_or(0));
        layouts.insert(package.id, package.bundles);
    }
    Ok(ContainerBundleLayout { layouts, load_orders })
}

/// d) Reversed post-order depth-first search. Roots are visited in `FPackageId` order,
/// which is the order of `neighbours`; a node already marked is skipped, which is what
/// breaks cycles; a node is appended once all of its neighbours are done, and the whole
/// list is reversed at the end.
fn reversed_post_order(neighbours: &[Vec<usize>]) -> Vec<usize> {
    let mut order = Vec::with_capacity(neighbours.len());
    let mut visited = vec![false; neighbours.len()];
    let mut stack: Vec<(usize, usize)> = Vec::new();

    for root in 0..neighbours.len() {
        if visited[root] {
            continue;
        }
        visited[root] = true;
        stack.push((root, 0));
        while let Some((node, cursor)) = stack.pop() {
            if cursor < neighbours[node].len() {
                stack.push((node, cursor + 1));
                let next = neighbours[node][cursor];
                if !visited[next] {
                    visited[next] = true;
                    stack.push((next, 0));
                }
            } else {
                order.push(node);
            }
        }
    }
    order.reverse();
    order
}
