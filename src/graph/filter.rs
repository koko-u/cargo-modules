// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;

use hir::HasAttrs;
use log::trace;
use petgraph::{
    graph::NodeIndex,
    stable_graph::EdgeIndex,
    visit::{Bfs, EdgeRef, IntoEdgeReferences},
    Direction,
};
use ra_ap_hir::{self as hir};
use ra_ap_ide_db::RootDatabase;
use ra_ap_syntax::ast;

use crate::graph::{
    edge::{Edge, EdgeKind},
    util, Graph,
};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Options {
    pub focus_on: Option<String>,
    pub max_depth: Option<usize>,
    pub acyclic: bool,
    pub types: bool,
    pub traits: bool,
    pub fns: bool,
    pub tests: bool,
    pub modules: bool,
    pub uses: bool,
    pub externs: bool,
}

#[derive(Debug)]
pub struct Filter<'a> {
    options: Options,
    db: &'a RootDatabase,
    krate: hir::Crate,
}

impl<'a> Filter<'a> {
    pub fn new(options: Options, db: &'a RootDatabase, krate: hir::Crate) -> Self {
        Self { options, db, krate }
    }

    pub fn filter(self, graph: &Graph, root_idx: NodeIndex) -> anyhow::Result<Graph> {
        const ROOT_DROP_ERR_MSG: &str = "Root module should not be dropped";

        let mut graph = graph.clone();

        let focus_on = self
            .options
            .focus_on
            .as_ref()
            .cloned()
            .unwrap_or_else(|| self.krate.display_name(self.db).unwrap().to_string());

        let syntax = format!("use {focus_on};");
        let use_tree: ast::UseTree = util::parse_ast(&syntax);

        trace!("Searching for focus nodes in graph ...");

        let focus_node_idxs: Vec<NodeIndex> = graph
            .node_indices()
            .filter(|node_idx| {
                let node = &graph[*node_idx];
                let node_path_segments = &node.path[..];
                if node_path_segments.is_empty() {
                    return false;
                }
                let node_path: ast::Path = {
                    let focus_on = node_path_segments.join("::");
                    let syntax = format!("use {focus_on};");
                    util::parse_ast(&syntax)
                };
                util::use_tree_matches_path(&use_tree, &node_path)
            })
            .collect();

        if focus_node_idxs.is_empty() {
            anyhow::bail!("No node found matching use tree '{:?}'", focus_on);
        }

        let max_depth = self.options.max_depth.unwrap_or(usize::MAX);
        let nodes_within_max_depth =
            util::nodes_within_max_depth_from(&graph, max_depth, &focus_node_idxs[..]);

        debug_assert!(
            nodes_within_max_depth.contains(&root_idx),
            "{}",
            ROOT_DROP_ERR_MSG
        );

        // Populate stack with owned nodes in breadth-first order:
        let mut stack: Vec<_> = {
            let mut stack: Vec<_> = Vec::default();

            let owner_only_graph = util::owner_only_graph(&graph);
            let mut traversal = Bfs::new(&owner_only_graph, root_idx);
            while let Some(node_idx) = traversal.next(&owner_only_graph) {
                stack.push(node_idx);
            }

            stack
        };

        trace!("Redirecting outgoing edges of filtered nodes in graph ...");

        // Popping from the stack results in a reverse level-order,
        // which ensures that sub-items are processed before their parent items:
        while let Some(node_idx) = stack.pop() {
            let node = &graph[node_idx];

            let Some(moduledef_hir) = node.hir else {
                continue;
            };

            let mut should_keep_node: bool = true;

            // Make sure the node is within our defined max depth:
            should_keep_node &= nodes_within_max_depth.contains(&node_idx);

            // Make sure the node's `moduledef` should be retained:
            should_keep_node &= self.should_retain_moduledef(moduledef_hir);

            // Make sure the root node doesn't get dropped:
            should_keep_node |= node_idx == root_idx;

            if should_keep_node {
                // If we're gonna keep the node then we can just keep it as is:
                continue;
            }

            // Try to find the single incoming "owns" edge:
            let parent_edge_ref =
                graph
                    .edges_directed(node_idx, Direction::Incoming)
                    .find(|edge_ref| {
                        let edge = edge_ref.weight();
                        matches!(edge.kind, EdgeKind::Owns)
                    });

            // And if one exists, then re-attach any outgoing edges to its source (i.e. parent item):
            if let Some(parent_edge_ref) = parent_edge_ref {
                let parent_node_idx = parent_edge_ref.source();

                // Collect edge indices and targets for outgoing "uses" edges:
                let pending: Vec<_> = graph
                    .edges_directed(node_idx, Direction::Outgoing)
                    .map(|outgoing_edge_ref| (outgoing_edge_ref.id(), outgoing_edge_ref.target()))
                    .collect();

                // Then replace the edge with one where the `source` is the parent, if necessary:
                for (edge_idx, target_node_idx) in pending {
                    let edge_weight = graph.remove_edge(edge_idx).unwrap();

                    graph.add_edge(parent_node_idx, target_node_idx, edge_weight);
                }
            }

            graph.remove_node(node_idx);
        }

        // Drop any "uses" edges, if necessary:
        if !self.options.uses {
            graph.retain_edges(|graph, edge_idx| {
                let edge = &graph[edge_idx];
                edge.kind == EdgeKind::Owns
            });
        }

        // The edge-reconciliation above may have resulted in redundant edges, so we need to remove those:

        let mut unique_edges: HashMap<(NodeIndex, NodeIndex, Edge), EdgeIndex> = HashMap::new();

        for edge_ref in graph.edge_references() {
            let source = edge_ref.source();
            let target = edge_ref.target();
            let weight = edge_ref.weight().clone();
            let idx = edge_ref.id();
            unique_edges.entry((source, target, weight)).or_insert(idx);
        }

        // Drop any redundant edges:

        graph.retain_edges(|graph, edge_idx| {
            let (source, target) = graph.edge_endpoints(edge_idx).unwrap();
            let weight = graph[edge_idx].clone();
            let idx = unique_edges[&(source, target, weight)];
            edge_idx == idx
        });

        // The above filters may have created disconnected sub-graphs.
        // We're only interested in the sub-graph containing the `root_idx` though,
        // so we query the graph for all node reachable from `root_node`:
        let nodes_reachable_from_root = util::nodes_reachable_from(&graph, root_idx);

        debug_assert!(
            nodes_reachable_from_root.contains(&root_idx),
            "{}",
            ROOT_DROP_ERR_MSG
        );

        // And drop any node that wasn't reachable from `root`:
        graph.retain_nodes(|_graph, node_idx| nodes_reachable_from_root.contains(&node_idx));

        debug_assert!(graph.contains_node(root_idx), "{}", ROOT_DROP_ERR_MSG);

        Ok(graph)
    }

    fn should_retain_moduledef(&self, moduledef_hir: hir::ModuleDef) -> bool {
        if !self.options.externs && self.is_extern(moduledef_hir) {
            return false;
        }

        match moduledef_hir {
            hir::ModuleDef::Module(module_hir) => self.should_retain_module(module_hir),
            hir::ModuleDef::Function(function_hir) => self.should_retain_function(function_hir),
            hir::ModuleDef::Adt(adt_hir) => self.should_retain_adt(adt_hir),
            hir::ModuleDef::Variant(variant_hir) => self.should_retain_variant(variant_hir),
            hir::ModuleDef::Const(const_hir) => self.should_retain_const(const_hir),
            hir::ModuleDef::Static(static_hir) => self.should_retain_static(static_hir),
            hir::ModuleDef::Trait(trait_hir) => self.should_retain_trait(trait_hir),
            hir::ModuleDef::TraitAlias(trait_alias_hir) => {
                self.should_retain_trait_alias(trait_alias_hir)
            }
            hir::ModuleDef::TypeAlias(type_alias_hir) => {
                self.should_retain_type_alias(type_alias_hir)
            }
            hir::ModuleDef::BuiltinType(builtin_type_hir) => {
                self.should_retain_builtin_type(builtin_type_hir)
            }
            hir::ModuleDef::Macro(macro_hir) => self.should_retain_macro(macro_hir),
        }
    }

    fn should_retain_module(&self, module_hir: hir::Module) -> bool {
        if !self.options.modules {
            // Always keep a crate's root module:
            return module_hir.is_crate_root();
        }
        true
    }

    fn should_retain_function(&self, function_hir: hir::Function) -> bool {
        if !self.options.fns {
            return false;
        }

        if !self.options.tests {
            let attrs = function_hir.attrs(self.db);
            if attrs.by_key("test").exists() {
                return false;
            }
        }

        true
    }

    fn should_retain_adt(&self, _adt_hir: hir::Adt) -> bool {
        if !self.options.types {
            return false;
        }

        true
    }

    fn should_retain_variant(&self, _variant_hir: hir::Variant) -> bool {
        false
    }

    fn should_retain_const(&self, _const_hir: hir::Const) -> bool {
        false
    }

    fn should_retain_static(&self, _static_hir: hir::Static) -> bool {
        false
    }

    fn should_retain_trait(&self, _trait_hir: hir::Trait) -> bool {
        if !self.options.traits {
            return false;
        }

        true
    }

    fn should_retain_trait_alias(&self, _trait_alias_hir: hir::TraitAlias) -> bool {
        false
    }

    fn should_retain_type_alias(&self, _type_alias_hir: hir::TypeAlias) -> bool {
        false
    }

    fn should_retain_builtin_type(&self, _builtin_type_hir: hir::BuiltinType) -> bool {
        if !self.options.types {
            return false;
        }

        true
    }

    fn should_retain_macro(&self, _macro_hir: hir::Macro) -> bool {
        false
    }

    fn is_extern(&self, moduledef_hir: hir::ModuleDef) -> bool {
        let module = if let hir::ModuleDef::Module(module_hir) = moduledef_hir {
            Some(module_hir)
        } else {
            moduledef_hir.module(self.db)
        };

        let Some(import_krate) = module.map(|module| module.krate()) else {
            return true;
        };

        self.krate != import_krate
    }
}
