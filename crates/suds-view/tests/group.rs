//! The groups of the stores.

use suds_view::{Grouping, Store, StoreStanding, store_groups};

const GIB: u64 = 1 << 30;

fn store(id: &str, names: [&'static str; 3], bytes: u64) -> Store {
    let [ecosystem, tool, class] = names;
    Store {
        id: id.to_owned(),
        ecosystem,
        tool,
        class,
        path: format!("/stores/{id}"),
        bytes,
        budget_bytes: None,
        standing: StoreStanding::Unbudgeted,
        share: 0.0,
    }
}

fn stores() -> Vec<Store> {
    const SHARED: &str = "Shared stores";
    let over = Store {
        budget_bytes: Some(10 * GIB),
        standing: StoreStanding::Over,
        ..store("pnpm.store", ["JavaScript", "pnpm", SHARED], 12 * GIB)
    };
    let absent = Store {
        standing: StoreStanding::Absent,
        ..store("lima.machines", ["Virtual machines", "lima", "Machines"], 0)
    };
    vec![
        store(
            "mise.installs",
            ["Many languages", "mise", "Toolchains"],
            15 * GIB,
        ),
        over,
        store("cargo.registry", ["Rust", "cargo", SHARED], 9 * GIB),
        store("sccache.cache", ["Rust", "sccache", SHARED], 7 * GIB),
        absent,
    ]
}

fn labels(groups: &[suds_view::StoreGroup]) -> Vec<&str> {
    groups.iter().map(|group| group.label.as_str()).collect()
}

#[test]
fn the_tools_of_one_language_form_one_ecosystem_and_the_largest_group_leads() {
    let groups = store_groups(&stores(), Grouping::Ecosystem);
    let expected = ["Rust", "Many languages", "JavaScript", "Virtual machines"];
    assert_eq!(labels(&groups), expected);
    assert_eq!(groups[0].bytes, 16 * GIB);
    assert_eq!(groups[0].share, 1.0);
    let ids: Vec<&str> = groups[0].stores.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["cargo.registry", "sccache.cache"]);
    assert_eq!(groups[2].over, 1);
    assert!((groups[2].share - 0.75).abs() < 0.001);
    assert!(groups[3].absent() && !groups[0].absent());
}

#[test]
fn each_tool_forms_a_group_of_its_own_beneath_its_ecosystem() {
    let groups = store_groups(&stores(), Grouping::Tool);
    let expected = ["mise", "pnpm", "cargo", "sccache", "lima"];
    assert_eq!(labels(&groups), expected);
    assert_eq!(groups[2].stores.len(), 1);
}

#[test]
fn stores_of_one_class_form_one_group() {
    let groups = store_groups(&stores(), Grouping::Class);
    assert_eq!(labels(&groups), ["Shared stores", "Toolchains", "Machines"]);
    assert_eq!(groups[0].bytes, 28 * GIB);
    assert_eq!((groups[0].over, groups[0].stores.len()), (1, 3));
    assert_eq!(groups[2].share, 0.0);
}

#[test]
fn no_store_gives_no_group() {
    assert!(store_groups(&[], Grouping::Ecosystem).is_empty());
}
