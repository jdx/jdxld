//! Builds a Mach-O exports trie.

use leb128::write::unsigned_len as uleb128_size;
use object::macho;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Symbol<'data> {
    pub(crate) name: &'data [u8],
    pub(crate) address: u64,
    pub(crate) flags: macho::ExportSymbolFlags,
}

#[derive(Debug, Default)]
struct Node {
    address: Option<u64>,
    flags: macho::ExportSymbolFlags,
    first_edge: usize,
    num_edges: usize,
    offset: usize,
    size: usize,
}

#[derive(Debug, Default)]
struct Edge<'data> {
    label: &'data [u8],
    child: usize,
    child_offset_size: usize,
}

#[derive(Debug, Default)]
struct UncompressedNode {
    symbol: Option<usize>,
    representative: usize,
    depth: usize,
    first_child: Option<usize>,
    last_child: Option<usize>,
    previous_sibling: Option<usize>,
    next_sibling: Option<usize>,
    num_children: usize,
}

#[derive(Debug, Default)]
pub(crate) struct Topology {
    nodes: Vec<UncompressedNode>,
}

#[cfg(test)]
fn build(symbols: &mut [Symbol<'_>]) -> Vec<u8> {
    symbols.sort_unstable_by(|a, b| a.name.cmp(b.name));
    build_sorted(symbols)
}

/// Build a Mach-O exports trie for symbols that are already sorted by name.
pub(crate) fn build_sorted(symbols: &[Symbol<'_>]) -> Vec<u8> {
    let topology = Topology::new(symbols);
    build_with_topology(symbols, &topology)
}

pub(crate) fn build_with_topology(symbols: &[Symbol<'_>], topology: &Topology) -> Vec<u8> {
    if symbols.is_empty() {
        return Vec::new();
    }

    debug_assert!(
        symbols.windows(2).all(|w| w[0].name < w[1].name),
        "Mach-O export symbol names are not sorted or contain duplicates"
    );

    let mut builder = Builder {
        symbols,
        nodes: Vec::with_capacity(topology.nodes.len()),
        edges: Vec::with_capacity(symbols.len()),
    };
    builder.build_nodes_from_topology(topology);
    builder.layout_until_stable();
    builder.encode()
}

pub(crate) fn size_with_topology(symbols: &[Symbol<'_>], topology: &Topology) -> usize {
    if symbols.is_empty() {
        return 0;
    }

    let mut builder = Builder {
        symbols,
        nodes: Vec::with_capacity(topology.nodes.len()),
        edges: Vec::with_capacity(symbols.len()),
    };
    builder.build_nodes_from_topology(topology);
    builder.layout_until_stable();
    builder.encoded_size()
}

struct Builder<'data, 'symbols> {
    symbols: &'symbols [Symbol<'data>],
    nodes: Vec<Node>,
    edges: Vec<Edge<'data>>,
}

impl Topology {
    pub(crate) fn new(symbols: &[Symbol<'_>]) -> Self {
        debug_assert!(
            symbols.windows(2).all(|w| w[0].name < w[1].name),
            "Mach-O export symbol names are not sorted or contain duplicates"
        );

        let mut uncompressed = vec![UncompressedNode::default()];
        let mut previous_name = &[][..];
        let mut previous_path = vec![0];

        for (symbol_index, symbol) in symbols.iter().enumerate() {
            let common_prefix = previous_name
                .iter()
                .zip(symbol.name)
                .take_while(|(a, b)| a == b)
                .count();

            while uncompressed[*previous_path.last().unwrap()].depth > common_prefix {
                previous_path.pop();
            }

            let mut parent = *previous_path.last().unwrap();
            if uncompressed[parent].depth < common_prefix {
                let previous_child = uncompressed[parent]
                    .last_child
                    .expect("previous symbol path is missing");
                let previous_sibling = uncompressed[previous_child].previous_sibling;
                let branch = uncompressed.len();
                uncompressed.push(UncompressedNode {
                    representative: symbol_index - 1,
                    depth: common_prefix,
                    first_child: Some(previous_child),
                    last_child: Some(previous_child),
                    previous_sibling,
                    num_children: 1,
                    ..Default::default()
                });
                uncompressed[previous_child].previous_sibling = None;
                if let Some(previous_sibling) = previous_sibling {
                    uncompressed[previous_sibling].next_sibling = Some(branch);
                } else {
                    uncompressed[parent].first_child = Some(branch);
                }
                uncompressed[parent].last_child = Some(branch);
                previous_path.push(branch);
                parent = branch;
            }

            if uncompressed[parent].depth == symbol.name.len() {
                uncompressed[parent].symbol = Some(symbol_index);
            } else {
                let child = uncompressed.len();
                uncompressed.push(UncompressedNode {
                    symbol: Some(symbol_index),
                    representative: symbol_index,
                    depth: symbol.name.len(),
                    ..Default::default()
                });
                append_child(&mut uncompressed, parent, child);
                previous_path.push(child);
            }
            previous_name = symbol.name;
        }

        Topology {
            nodes: uncompressed,
        }
    }
}

impl<'data> Builder<'data, '_> {
    #[cfg(test)]
    fn build_nodes(&mut self) {
        let topology = Topology::new(self.symbols);
        self.build_nodes_from_topology(&topology);
    }

    fn build_nodes_from_topology(&mut self, topology: &Topology) {
        let uncompressed = &topology.nodes;
        for source in uncompressed {
            let (address, flags) = source
                .symbol
                .map_or((None, macho::ExportSymbolFlags(0)), |i| {
                    (Some(self.symbols[i].address), self.symbols[i].flags)
                });

            let first_edge = self.edges.len();
            debug_assert!(
                u8::try_from(source.num_children).is_ok(),
                "Mach-O exports trie node has too many children"
            );

            self.nodes.push(Node {
                address,
                flags,
                first_edge,
                num_edges: source.num_children,
                ..Default::default()
            });

            let mut child = source.first_child;
            while let Some(child_index) = child {
                let child_node = &uncompressed[child_index];

                self.edges.push(Edge {
                    label: &self.symbols[child_node.representative].name
                        [source.depth..child_node.depth],
                    child: child_index,
                    child_offset_size: 1,
                });
                child = child_node.next_sibling;
            }
        }
    }

    fn layout_until_stable(&mut self) {
        loop {
            let mut offset = 0;

            for index in 0..self.nodes.len() {
                self.nodes[index].offset = offset;
                self.nodes[index].size = self.node_size(index);
                offset += self.nodes[index].size;
            }

            let mut changed = false;

            for edge in &mut self.edges {
                let offset_size = uleb128_size(self.nodes[edge.child].offset as u64);
                if edge.child_offset_size != offset_size {
                    edge.child_offset_size = offset_size;
                    changed = true;
                }
            }

            if !changed {
                break;
            }
        }
    }

    fn node_size(&self, node_index: usize) -> usize {
        let node = &self.nodes[node_index];
        let terminal_size = node
            .address
            .map_or(0, |address| regular_export_size(node.flags, address));
        uleb128_size(terminal_size as u64)
            + terminal_size
            + 1
            + self
                .node_edges(node_index)
                .map(|edge| edge.label.len() + 1 + edge.child_offset_size)
                .sum::<usize>()
    }

    fn encode(&self) -> Vec<u8> {
        let total_size = self.encoded_size();
        let mut out = Vec::with_capacity(total_size);

        for (node_index, node) in self.nodes.iter().enumerate() {
            debug_assert_eq!(out.len(), node.offset);

            if let Some(address) = node.address {
                write_uleb128(&mut out, regular_export_size(node.flags, address) as u64);
                write_regular_export(&mut out, node.flags, address);
            } else {
                write_uleb128(&mut out, 0);
            }

            out.push(node.num_edges as u8);

            for edge in self.node_edges(node_index) {
                out.extend_from_slice(edge.label);
                out.push(0);
                write_uleb128(&mut out, self.nodes[edge.child].offset as u64);
            }
        }

        debug_assert_eq!(out.len(), total_size);
        out
    }

    fn encoded_size(&self) -> usize {
        self.nodes.last().map_or(0, |node| node.offset + node.size)
    }

    fn node_edges(&self, node_index: usize) -> impl Iterator<Item = &Edge<'data>> {
        let node = &self.nodes[node_index];
        self.edges[node.first_edge..node.first_edge + node.num_edges].iter()
    }
}

fn append_child(nodes: &mut [UncompressedNode], parent: usize, child: usize) {
    let previous_sibling = nodes[parent].last_child;
    nodes[child].previous_sibling = previous_sibling;
    if let Some(previous_sibling) = previous_sibling {
        nodes[previous_sibling].next_sibling = Some(child);
    } else {
        nodes[parent].first_child = Some(child);
    }
    nodes[parent].last_child = Some(child);
    nodes[parent].num_children += 1;
}

fn regular_export_size(flags: macho::ExportSymbolFlags, address: u64) -> usize {
    uleb128_size(flags.0) + uleb128_size(address)
}

fn write_regular_export(out: &mut Vec<u8>, flags: macho::ExportSymbolFlags, address: u64) {
    write_uleb128(out, flags.0);
    write_uleb128(out, address);
}

fn write_uleb128(out: &mut Vec<u8>, value: u64) {
    leb128::write::unsigned(out, value).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use itertools::Itertools;
    use object::LittleEndian;
    use object::macho;
    use object::read::macho::ExportData;

    #[derive(Debug, PartialEq, Eq)]
    struct ParsedSymbol {
        name: Vec<u8>,
        address: u64,
        flags: macho::ExportSymbolFlags,
    }

    fn check(symbols: &mut [Symbol]) {
        let trie = build(symbols);
        assert_eq!(trie, build_sorted(symbols));

        assert_eq!(
            parse_exports(&trie),
            symbols
                .iter()
                .map(|s| ParsedSymbol {
                    name: s.name.to_owned(),
                    address: s.address,
                    flags: s.flags,
                })
                .collect_vec()
        );
    }

    fn parse_exports(data: &[u8]) -> Vec<ParsedSymbol> {
        if data.is_empty() {
            return Vec::new();
        }

        let command = macho::LinkeditDataCommand {
            cmd: macho::LC_DYLD_EXPORTS_TRIE.into(),
            cmdsize: (size_of::<macho::LinkeditDataCommand<object::Endianness>>() as u32).into(),
            dataoff: 0.into(),
            datasize: (data.len() as u32).into(),
        };

        command
            .exports_trie(LittleEndian, data)
            .unwrap()
            .map(|symbol| {
                let symbol = symbol.unwrap();
                let ExportData::Regular { address } = symbol.data() else {
                    panic!("expected regular export");
                };
                ParsedSymbol {
                    name: symbol.name().to_vec(),
                    address: *address,
                    flags: symbol.flags(),
                }
            })
            .collect()
    }

    #[test]
    fn empty_input_produces_empty_trie() {
        let mut symbols = [];
        assert_eq!(build(&mut symbols), []);
    }

    #[test]
    fn builds_single_symbol_trie() {
        check(&mut [Symbol {
            name: b"_main",
            address: 0x1234,
            flags: macho::ExportSymbolFlags(0),
        }]);
    }

    #[test]
    fn builds_absolute_symbol() {
        let mut symbols = [Symbol {
            name: b"_absolute",
            address: 42,
            flags: macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE.into(),
        }];

        check(&mut symbols);
    }

    #[test]
    fn builds_weak_symbol() {
        let mut symbols = [Symbol {
            name: b"_weak",
            address: 42,
            flags: macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION,
        }];

        check(&mut symbols);
    }

    #[test]
    fn builds_shared_prefix_trie() {
        check(&mut [
            Symbol {
                name: b"_foobar",
                address: 1,
                flags: macho::ExportSymbolFlags(0),
            },
            Symbol {
                name: b"_foo",
                address: 2,
                flags: macho::ExportSymbolFlags(0),
            },
            Symbol {
                name: b"_fop",
                address: 3,
                flags: macho::ExportSymbolFlags(0),
            },
        ]);
    }

    #[test]
    fn builds_deeply_nested_prefixes() {
        let names = (1..=1024).map(|len| vec![b'a'; len]).collect_vec();
        let mut symbols = names
            .iter()
            .enumerate()
            .map(|(address, name)| Symbol {
                name,
                address: address as u64,
                flags: macho::ExportSymbolFlags(0),
            })
            .collect_vec();

        check(&mut symbols);

        let mut builder = Builder {
            symbols: &symbols,
            nodes: Vec::new(),
            edges: Vec::new(),
        };
        builder.build_nodes();
        assert_eq!(builder.nodes.len(), symbols.len() + 1);
    }

    #[test]
    fn every_non_zero_byte() {
        let names: Vec<Vec<u8>> = (1..=255).map(|n| vec![n]).collect();
        let mut symbols: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(index, name)| Symbol {
                name,
                address: index as u64,
                flags: macho::ExportSymbolFlags(0),
            })
            .collect();

        check(&mut symbols);
    }

    #[test]
    fn maximum_addresses_give_a_conservative_size() {
        let names = (0..512)
            .map(|index| format!("_shared_prefix_{index:04x}").into_bytes())
            .collect_vec();

        let mut actual = names
            .iter()
            .enumerate()
            .map(|(index, name)| Symbol {
                name,
                address: 1_u64 << (index % 63),
                flags: macho::ExportSymbolFlags(0),
            })
            .collect_vec();

        let mut maximum = names
            .iter()
            .map(|name| Symbol {
                name,
                address: u64::MAX,
                flags: macho::ExportSymbolFlags(0),
            })
            .collect_vec();

        let maximum_trie = build(&mut maximum);
        let topology = Topology::new(&maximum);
        assert_eq!(size_with_topology(&maximum, &topology), maximum_trie.len());
        actual.sort_unstable_by(|a, b| a.name.cmp(b.name));
        let actual_trie = build_with_topology(&actual, &topology);

        assert_eq!(actual_trie, build_sorted(&actual));
        assert!(actual_trie.len() <= maximum_trie.len());
    }
}
