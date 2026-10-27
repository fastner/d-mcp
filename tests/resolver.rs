//! Property test: resolution is deterministic for shuffled inputs.

use d_mcp::config::{HarnessSet, Selection};
use d_mcp::definition::{OverrideSites, parse_definition};
use d_mcp::resolver::{FileEntry, FileProblem, SourceSnapshot, resolve};
use d_mcp::rule::Rule;
use proptest::prelude::*;

const NAMES: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

/// A generated file: index into `NAMES` (or `None` for an invalid stem),
/// and whether it is valid.
type FileSpec = (Option<usize>, bool, u8);

fn file(spec: &FileSpec, position: usize) -> FileEntry {
    let (name, valid, variant) = *spec;
    match name {
        None => FileEntry {
            file: format!("servers/Bad{position}.toml"),
            key: None,
            outcome: Err(FileProblem::file(Rule::InvalidStem)),
            override_sites: OverrideSites::default(),
        },
        Some(index) => {
            let name = NAMES[index];
            let outcome = if valid {
                Ok(parse_definition(
                    &format!(
                        "name = \"{name}\"\n[transport]\ntype = \"stdio\"\ncommand = \"c{variant}\"\n"
                    ),
                    name,
                )
                .unwrap())
            } else {
                Err(FileProblem::file(Rule::TomlSyntax))
            };
            FileEntry {
                file: format!("servers/{name}.toml"),
                key: Some(name.to_owned()),
                outcome,
                override_sites: OverrideSites::default(),
            }
        }
    }
}

fn sources() -> impl Strategy<Value = Vec<SourceSnapshot>> {
    let source = (
        any::<bool>(),
        prop::collection::vec(
            (prop::option::of(0..NAMES.len()), any::<bool>(), any::<u8>()),
            0..5,
        ),
    );
    prop::collection::vec(source, 1..5).prop_map(|specs| {
        specs
            .into_iter()
            .enumerate()
            .map(|(index, (available, files))| {
                // Deduplicate keys per source: a directory cannot hold two
                // files with the same stem.
                let mut seen = std::collections::BTreeSet::new();
                let files: Vec<FileEntry> = files
                    .iter()
                    .enumerate()
                    .filter(|(_, spec)| spec.0.is_none_or(|name| seen.insert(name)))
                    .map(|(position, spec)| file(spec, position))
                    .collect();
                SourceSnapshot {
                    id: format!("s{index}"),
                    priority: u32::try_from(index).unwrap() * 10,
                    available: available || index == 0,
                    files: if available || index == 0 {
                        files
                    } else {
                        Vec::new()
                    },
                    notices: Vec::new(),
                }
            })
            .collect()
    })
}

fn selections() -> impl Strategy<Value = Vec<Selection>> {
    prop::collection::btree_set(0..NAMES.len() + 1, 0..5).prop_map(|indexes| {
        indexes
            .into_iter()
            .map(|index| Selection {
                server: NAMES.get(index).copied().unwrap_or("ghost").to_owned(),
                harnesses: HarnessSet::All,
            })
            .collect()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn resolution_is_deterministic_for_shuffled_inputs(
        sources in sources(),
        selections in selections(),
        seed in any::<u64>(),
    ) {
        let expected = resolve(&sources, &selections);
        // Shuffle sources, files and selections with a simple LCG.
        let mut state = seed | 1;
        let mut next = move || {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            usize::try_from(state >> 33).unwrap()
        };
        let mut shuffled_sources = sources;
        for index in (1..shuffled_sources.len()).rev() {
            shuffled_sources.swap(index, next() % (index + 1));
        }
        for source in &mut shuffled_sources {
            for index in (1..source.files.len()).rev() {
                source.files.swap(index, next() % (index + 1));
            }
        }
        let mut shuffled_selections = selections.clone();
        for index in (1..shuffled_selections.len()).rev() {
            shuffled_selections.swap(index, next() % (index + 1));
        }
        let actual = resolve(&shuffled_sources, &shuffled_selections);
        prop_assert_eq!(&actual, &expected);
        prop_assert_eq!(actual.servers.len(), selections.len());
    }
}
