# An identity format v2 graph (synthetic)

A small graph in mdkg 0.6.0's identity format v2, for `migrate`'s v2
reader (`crates/mdkg-graph/src/identity_v2.rs`, format 0.7 section 10.2).
All content is invented. It has a manifest (`.mdkg/graph.json`), config
`schema_version` 2, `graph_id` and `node_id` in every node (version 4
UUIDs, as 0.6.0 minted them), and identity refs `mdkg://<graph>/<node>`
between nodes and to another graph.
