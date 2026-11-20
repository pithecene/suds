//! The text and the figures of each item in the window, from the view model.

use slint::{Color, SharedString};
use suds_core::pressure::Level;
use suds_view::{Checkout, Class, PressureCard, Store, StoreStanding, bytes, day};

use crate::{BuildItem, CheckoutItem, ClassItem, FactItem, StoreItem};

/// One tint for each class, in the order of the view model's classes.
const TINTS: [u32; 6] = [0x4c7fe0, 0x8a72f0, 0xe0951c, 0x2f9e6b, 0xd9659a, 0x3fb1c4];

fn tint(position: usize) -> Color {
    let rgb = TINTS[position % TINTS.len()];
    Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub fn level(level: Level) -> (&'static str, i32) {
    match level {
        Level::Normal => ("Normal", 0),
        Level::Soft => ("Soft pressure", 1),
        Level::Hard => ("Hard pressure", 2),
    }
}

/// The line beneath the pressure's meter.
pub fn pressure_facts(card: &PressureCard) -> String {
    let mut facts = vec![format!("{:.0}% of the ceiling", card.share * 100.0)];
    if let Some(free) = card.filesystem_available_bytes {
        facts.push(format!("{} free on the filesystem", bytes(free)));
    }
    facts.push(if card.enforced {
        "the filesystem enforces the ceiling".to_owned()
    } else {
        "no filesystem enforces the ceiling".to_owned()
    });
    if !card.complete {
        facts.push("a lower bound, because a root left entries unread".to_owned());
    }
    facts.join(" · ")
}

/// What a reading left unread, or nothing for a whole reading.
pub fn notice(error_count: u64, registry_error: Option<&str>) -> String {
    let mut lines = Vec::new();
    if error_count > 0 {
        lines.push(format!(
            "Suds could not read {error_count} entries, so each figure is a lower bound."
        ));
    }
    if let Some(reason) = registry_error {
        lines.push(format!(
            "Suds could not read the registry, so no checkout shows its hold: {reason}"
        ));
    }
    lines.join("\n")
}

pub fn class(position: usize, class: &Class) -> ClassItem {
    ClassItem {
        label: class.label.into(),
        detail: format!("{} · {}", bytes(class.bytes), class.count).into(),
        share: class.share,
        tint: tint(position),
    }
}

fn hold(checkout: &Checkout) -> SharedString {
    match &checkout.hold {
        None => SharedString::new(),
        Some(hold) if hold.released => "released".into(),
        Some(hold) => hold.client.as_str().into(),
    }
}

fn build(checkout: &Checkout) -> String {
    if checkout.build_state.is_empty() {
        "no build state".to_owned()
    } else {
        format!("{} build", bytes(checkout.build_bytes))
    }
}

/// A path with the home directory written as a tilde, as a shell prints one.
pub fn short(path: &str, home: Option<&str>) -> String {
    match home.and_then(|home| path.strip_prefix(home)) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

pub fn checkout(checkout: &Checkout, home: Option<&str>) -> CheckoutItem {
    CheckoutItem {
        name: checkout.name.as_str().into(),
        parent: short(&checkout.parent, home).into(),
        kind: if checkout.linked { "linked" } else { "main" }.into(),
        total: bytes(checkout.total_bytes()).into(),
        build: build(checkout).into(),
        modified: day(&checkout.newest_modified).into(),
        share: checkout.share,
        hold: hold(checkout),
    }
}

/// The facts of one checkout, each with its label, for the detail pane.
pub fn facts(checkout: &Checkout) -> Vec<FactItem> {
    let fact = |label: &str, value: String| FactItem {
        label: label.into(),
        value: value.into(),
    };
    let mut facts = vec![
        fact(
            "Kind",
            if checkout.linked {
                "Linked checkout"
            } else {
                "Main checkout"
            }
            .to_owned(),
        ),
        fact("Source", bytes(checkout.own_bytes)),
        fact("Build state", build(checkout)),
        fact("Last change", day(&checkout.newest_modified).to_owned()),
    ];
    if let Some(repository) = &checkout.repository {
        facts.insert(1, fact("Repository", repository.clone()));
    }
    facts.push(match &checkout.hold {
        None => fact("Hold", "No client holds it".to_owned()),
        Some(hold) if hold.released => fact(
            "Hold",
            match &hold.landed {
                Some(_) => format!(
                    "{} released it as landed, {}",
                    hold.client,
                    day(&hold.since)
                ),
                None => format!("{} released it, {}", hold.client, day(&hold.since)),
            },
        ),
        Some(hold) => fact(
            "Hold",
            format!("{} holds it since {}", hold.client, day(&hold.since)),
        ),
    });
    facts
}

pub fn builds(checkout: &Checkout) -> Vec<BuildItem> {
    checkout
        .build_state
        .iter()
        .map(|build| BuildItem {
            // The pane shows a build directory's path beneath its checkout.
            path: build
                .path
                .strip_prefix(&checkout.path)
                .map_or(build.path.as_str(), |rest| rest.trim_start_matches('/'))
                .into(),
            ecosystem: build.ecosystem.into(),
            size: bytes(build.bytes).into(),
            modified: day(&build.newest_modified).into(),
        })
        .collect()
}

pub fn store(store: &Store) -> StoreItem {
    let budget = match (store.standing, store.budget_bytes) {
        (StoreStanding::Absent, _) => "absent".to_owned(),
        (StoreStanding::Unreadable, _) => "unreadable".to_owned(),
        (StoreStanding::Over, Some(budget)) => format!("over its budget of {}", bytes(budget)),
        (_, Some(budget)) => format!("budget {}", bytes(budget)),
        (_, None) => "no budget".to_owned(),
    };
    StoreItem {
        id: store.id.as_str().into(),
        path: store.path.as_str().into(),
        size: bytes(store.bytes).into(),
        budget: budget.into(),
        share: store.share,
        over: store.standing == StoreStanding::Over,
        absent: matches!(
            store.standing,
            StoreStanding::Absent | StoreStanding::Unreadable
        ),
    }
}

#[cfg(test)]
mod tests {
    use suds_view::{BuildState, Checkout, Hold, Store, StoreStanding};

    use super::{builds, checkout, facts, notice, short, store};

    #[test]
    fn a_path_beneath_the_home_directory_opens_with_a_tilde() {
        assert_eq!(short("/home/a/work/app", Some("/home/a")), "~/work/app");
        assert_eq!(short("/home/a", Some("/home/a")), "~");
        // A sibling whose name opens with the home's name stays whole.
        assert_eq!(short("/home/abc", Some("/home/a")), "/home/abc");
        assert_eq!(short("/srv/app", None), "/srv/app");
    }

    fn fixture() -> Checkout {
        Checkout {
            path: "/work/app".into(),
            name: "app".into(),
            parent: "/work".into(),
            linked: true,
            repository: Some("/work/main".into()),
            own_bytes: 1024,
            build_bytes: 2048,
            build_state: vec![BuildState {
                path: "/work/app/target".into(),
                ecosystem: "cargo",
                bytes: 2048,
                newest_modified: "2026-11-20T00:00:00Z".into(),
            }],
            newest_modified: "2026-11-19T08:00:00Z".into(),
            hold: Some(Hold {
                client: "bosun".into(),
                since: "2026-11-18T00:00:00Z".into(),
                released: true,
                landed: Some("abc".into()),
            }),
            share: 0.5,
        }
    }

    #[test]
    fn a_row_states_its_size_its_build_state_and_its_hold() {
        let row = checkout(&fixture(), Some("/work"));
        assert_eq!(row.parent, "~");
        assert_eq!(row.kind, "linked");
        assert_eq!(row.total, "3.0 KiB");
        assert_eq!(row.build, "2.0 KiB build");
        assert_eq!(row.modified, "2026-11-19");
        assert_eq!(row.hold, "released");
    }

    #[test]
    fn the_detail_names_the_repository_the_release_and_each_build_directory() {
        let checkout = fixture();
        let labels: Vec<String> = facts(&checkout)
            .iter()
            .map(|f| f.label.to_string())
            .collect();
        assert_eq!(
            labels,
            [
                "Kind",
                "Repository",
                "Source",
                "Build state",
                "Last change",
                "Hold"
            ]
        );
        let hold = facts(&checkout).pop().unwrap();
        assert_eq!(hold.value, "bosun released it as landed, 2026-11-18");
        let build = &builds(&checkout)[0];
        assert_eq!(build.path, "target");
        assert_eq!(build.size, "2.0 KiB");
    }

    #[test]
    fn a_notice_states_each_part_that_a_reading_left_unread() {
        assert_eq!(notice(0, None), "");
        assert!(notice(3, None).contains("3 entries"));
        let both = notice(1, Some("permission denied"));
        assert_eq!(both.lines().count(), 2);
        assert!(both.ends_with("no checkout shows its hold: permission denied"));
    }

    #[test]
    fn a_store_over_its_budget_says_so() {
        let item = store(&Store {
            id: "go.module-cache".into(),
            path: "/stores/go".into(),
            bytes: 11 << 30,
            budget_bytes: Some(10 << 30),
            standing: StoreStanding::Over,
            share: 1.0,
        });
        assert!(item.over);
        assert_eq!(item.budget, "over its budget of 10.0 GiB");
    }
}
