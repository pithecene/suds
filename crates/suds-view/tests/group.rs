//! The groups of the stores.

use suds_view::{Grouping, Store, StoreStanding, store_groups};

const GIB: u64 = 1 << 30;

fn store(id: &str, ecosystem: &str, class: &str, bytes: u64) -> Store {
    Store {
        id: id.to_owned(),
        ecosystem: ecosystem.to_owned(),
        class: class.to_owned(),
        path: format!("/stores/{id}"),
        bytes,
        budget_bytes: None,
        standing: StoreStanding::Unbudgeted,
        share: 0.0,
    }
}

fn stores() -> Vec<Store> {
    let over = Store {
        budget_bytes: Some(10 * GIB),
        standing: StoreStanding::Over,
        ..store("pnpm.store", "pnpm", "Shared stores", 12 * GIB)
    };
    let absent = Store {
        standing: StoreStanding::Absent,
        ..store("lima.machines", "lima", "Machines", 0)
    };
    vec![
        store("mise.installs", "mise", "Toolchains", 15 * GIB),
        over,
        store("go.module-cache", "go", "Shared stores", 9 * GIB),
        store("go.build-cache", "go", "Shared stores", 7 * GIB),
        absent,
    ]
}

#[test]
fn stores_of_one_ecosystem_form_one_group_and_the_largest_group_leads() {
    let groups = store_groups(&stores(), Grouping::Ecosystem);
    let labels: Vec<&str> = groups.iter().map(|group| group.label.as_str()).collect();
    assert_eq!(labels, ["go", "mise", "pnpm", "lima"]);
    assert_eq!(groups[0].bytes, 16 * GIB);
    assert_eq!(groups[0].share, 1.0);
    let ids: Vec<&str> = groups[0].stores.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["go.module-cache", "go.build-cache"]);
    assert_eq!(groups[2].over, 1);
    assert!((groups[2].share - 0.75).abs() < 0.001);
    assert!(groups[3].absent() && !groups[0].absent());
}

#[test]
fn stores_of_one_class_form_one_group() {
    let groups = store_groups(&stores(), Grouping::Class);
    let labels: Vec<&str> = groups.iter().map(|group| group.label.as_str()).collect();
    assert_eq!(labels, ["Shared stores", "Toolchains", "Machines"]);
    assert_eq!(groups[0].bytes, 28 * GIB);
    assert_eq!((groups[0].over, groups[0].stores.len()), (1, 3));
    assert_eq!(groups[2].share, 0.0);
}

#[test]
fn no_store_gives_no_group() {
    assert!(store_groups(&[], Grouping::Ecosystem).is_empty());
}
