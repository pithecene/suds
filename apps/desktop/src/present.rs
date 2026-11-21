//! The text and the figures of each item in the window, from the view model.

use slint::{Color, SharedString};
use suds_core::pressure::Level;
use suds_core::schedule::Standing;
use suds_view::{
    Checkout, Class, Frame, PressureCard, Project, Rect, Store, StoreStanding, Tile, Upkeep, bytes,
    day,
};

use crate::{BuildItem, ClassItem, FactItem, FrameItem, RowItem, StoreItem, TileItem};

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

/// A reading's time of day. The reading states its time in RFC 3339, at UTC.
pub fn observed(at: &str) -> String {
    match at.get(11..16) {
        Some(time) => format!("{time} UTC"),
        None => at.to_owned(),
    }
}

/// The line beneath the pressure's meter.
pub fn pressure_facts(card: &PressureCard) -> String {
    // The meter's share stops at the whole track, and the text states the usage beyond the ceiling.
    let percent = card.usage_bytes as f64 / card.ceiling_bytes.max(1) as f64 * 100.0;
    let mut facts = vec![format!("{percent:.0}% of the ceiling")];
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

/// What the scheduled maintenance needs from a person, or nothing for a sound schedule.
/// A scheduled run that fails tells nobody, so the window states it.
pub fn upkeep(upkeep: &Upkeep) -> String {
    const INSTALL: &str =
        "The command suds schedule --install plans a new one, and --apply places it.";
    let mut lines = Vec::new();
    match &upkeep.schedule {
        Standing::Current => {}
        Standing::Absent => lines.push(format!(
            "No schedule runs the maintenance, so nothing holds the storage to its ceiling without a person. {INSTALL}"
        )),
        Standing::Stale(reason) => lines.push(format!(
            "The maintenance schedule is stale: {reason}. {INSTALL}"
        )),
        Standing::Unreadable(reason) => lines.push(format!(
            "Suds could not read the maintenance schedule: {reason}."
        )),
    }
    if let Some(reason) = &upkeep.journal_error {
        lines.push(format!(
            "Suds could not read the journal, so the last scheduled run is unknown: {reason}"
        ));
    }
    if upkeep.overdue {
        lines.push(match &upkeep.last_run {
            Some(at) => format!(
                "The scheduled maintenance last ran on {}, more than a day ago.",
                day(at)
            ),
            None => "The scheduled maintenance has not run yet.".to_owned(),
        });
    }
    for trim in &upkeep.failed_trims {
        lines.push(format!(
            "The last scheduled run could not trim {}: {}.",
            trim.store, trim.reason
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

/// A checkout's standing with the client that registered it, in a few words, and the standing's rank.
/// The ranks are 0 for none, 1 for a checkout in use, 2 for one whose work landed, and 3 for one let go.
fn state(checkout: &Checkout) -> (String, i32) {
    match &checkout.hold {
        None => (String::new(), 0),
        Some(hold) if !hold.released => (format!("in use by {}", hold.client), 1),
        Some(hold) if hold.landed.is_some() => ("landed".to_owned(), 2),
        Some(_) => ("finished".to_owned(), 3),
    }
}

/// A checkout's standing as a sentence, for the detail pane.
fn standing(checkout: &Checkout) -> String {
    match &checkout.hold {
        None => "Not in use by any tool.".to_owned(),
        Some(hold) if !hold.released => format!(
            "In use by {} since {}. Suds leaves it untouched.",
            hold.client,
            day(&hold.since)
        ),
        Some(hold) if hold.landed.is_some() => format!(
            "{} finished with it on {}, and its work has landed. Suds can remove it once it is clean and idle.",
            hold.client,
            day(&hold.since)
        ),
        Some(hold) => format!("{} finished with it on {}.", hold.client, day(&hold.since)),
    }
}

fn build(bytes: u64) -> String {
    if bytes == 0 {
        "no build state".to_owned()
    } else {
        format!("{} build", suds_view::bytes(bytes))
    }
}

/// A path with the home directory written as a tilde, as a shell prints one.
pub fn short(path: &str, home: Option<&str>) -> String {
    match home.and_then(|home| path.strip_prefix(home)) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

fn share(part: u64, whole: u64) -> f32 {
    if whole == 0 {
        0.0
    } else {
        (part as f64 / whole as f64) as f32
    }
}

pub fn count(number: usize, one: &str, many: &str) -> String {
    format!("{number} {}", if number == 1 { one } else { many })
}

/// The tint of a checkout's source: one for a repository's main checkout, one for a worktree.
fn source_tint(linked: bool) -> Color {
    tint(usize::from(linked))
}

/// The count beside the outline's heading: the projects, and the checkouts that the filter keeps.
pub fn counts(projects: usize, kept: usize, all: usize) -> String {
    let checkouts = if kept == all {
        count(all, "checkout", "checkouts")
    } else {
        format!("{kept} of {all} checkouts")
    };
    format!("{} · {checkouts}", count(projects, "project", "projects"))
}

/// A project's row of the outline.
pub fn project_row(project: &Project, open: bool, home: Option<&str>) -> RowItem {
    let mut caption = count(project.members.len(), "checkout", "checkouts");
    if project.clones > 1 {
        caption.push_str(&format!(" · {} clones", project.clones));
    }
    if project.held > 0 {
        caption.push_str(&format!(" · {} in use", project.held));
    }
    RowItem {
        project: true,
        open,
        name: project.name.as_str().into(),
        caption: format!("{caption} · {}", short(&project.key, home)).into(),
        total: bytes(project.total_bytes()).into(),
        build: build(project.build_bytes).into(),
        modified: day(&project.newest_modified).into(),
        share: project.share,
        build_share: share(project.build_bytes, project.total_bytes()),
        state: SharedString::new(),
        state_rank: 0,
        tint: source_tint(false),
    }
}

/// A checkout's row of the outline, beneath its project's. Each bar of the outline measures
/// against `largest`, the size of the largest project.
pub fn checkout_row(checkout: &Checkout, largest: u64, home: Option<&str>) -> RowItem {
    let (state, state_rank) = state(checkout);
    RowItem {
        project: false,
        open: false,
        name: checkout.name.as_str().into(),
        caption: if checkout.linked {
            format!("worktree · {}", short(&checkout.parent, home))
        } else {
            format!("clone · {}", short(&checkout.parent, home))
        }
        .into(),
        total: bytes(checkout.total_bytes()).into(),
        build: build(checkout.build_bytes).into(),
        modified: day(&checkout.newest_modified).into(),
        share: share(checkout.total_bytes(), largest),
        build_share: share(checkout.build_bytes, checkout.total_bytes()),
        state: state.into(),
        state_rank,
        tint: source_tint(checkout.linked),
    }
}

fn fact(label: &str, value: String) -> FactItem {
    FactItem {
        label: label.into(),
        value: value.into(),
    }
}

/// The facts of one checkout, each with its label, for the detail pane.
pub fn facts(checkout: &Checkout, home: Option<&str>) -> Vec<FactItem> {
    let mut facts = vec![
        fact(
            "Kind",
            if checkout.linked {
                "Worktree: a second checkout of its repository"
            } else {
                "Clone: the repository's own checkout"
            }
            .to_owned(),
        ),
        fact("Source", bytes(checkout.own_bytes)),
        fact("Build state", build(checkout.build_bytes)),
        fact("Last change", day(&checkout.newest_modified).to_owned()),
        fact("Status", standing(checkout)),
    ];
    if let Some(repository) = &checkout.repository {
        facts.insert(1, fact("Repository", short(repository, home)));
    }
    facts
}

/// The facts of one project, each with its label, for the detail pane.
pub fn project_facts(project: &Project) -> Vec<FactItem> {
    vec![
        fact(
            "Checkouts",
            format!(
                "{} in {}",
                count(project.members.len(), "checkout", "checkouts"),
                count(project.clones, "clone", "clones")
            ),
        ),
        fact("In use", format!("{} of them", project.held)),
        fact("Source", bytes(project.own_bytes)),
        fact("Build state", build(project.build_bytes)),
        fact("Last change", day(&project.newest_modified).to_owned()),
    ]
}

fn place(rect: &Rect) -> (f32, f32, f32, f32) {
    (rect.x, rect.y, rect.width, rect.height)
}

/// One project's frame on the storage map.
pub fn frame(frame: &Frame, project: &Project) -> FrameItem {
    let (x, y, w, h) = place(&frame.rect);
    let (bx, by, bw, bh) = place(&frame.build);
    FrameItem {
        // The map draws no frame for an empty project, so a frame names its project itself.
        id: i32::try_from(frame.project).unwrap_or(-1),
        x,
        y,
        w,
        h,
        bx,
        by,
        bw,
        bh,
        name: project.name.as_str().into(),
        tip: format!(
            "{} · {} · {} · {}",
            project.name,
            count(project.members.len(), "checkout", "checkouts"),
            bytes(project.total_bytes()),
            build(project.build_bytes)
        )
        .into(),
        labelled: frame.labelled,
        detailed: frame.detailed,
    }
}

/// One checkout's tile on the storage map.
pub fn tile(tile: &Tile, checkout: &Checkout, project: &Project) -> TileItem {
    let (x, y, w, h) = place(&tile.rect);
    let (bx, by, bw, bh) = place(&tile.build);
    let (state, _) = state(checkout);
    let mut tip = format!(
        "{} / {} · {} · {}",
        project.name,
        checkout.name,
        bytes(checkout.total_bytes()),
        build(checkout.build_bytes)
    );
    if !state.is_empty() {
        tip.push_str(&format!(" · {state}"));
    }
    TileItem {
        x,
        y,
        w,
        h,
        bx,
        by,
        bw,
        bh,
        id: i32::try_from(tile.checkout).unwrap_or(-1),
        name: checkout.name.as_str().into(),
        tip: tip.into(),
        tint: source_tint(checkout.linked),
    }
}

/// A checkout's build directories for the detail pane, largest first.
pub fn builds(checkout: &Checkout) -> Vec<BuildItem> {
    let mut builds: Vec<_> = checkout.build_state.iter().collect();
    builds.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    builds
        .into_iter()
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
    use suds_core::pressure::Level;
    use suds_view::{
        BuildState, Checkout, Hold, PressureCard, Sort, Store, StoreStanding, map, projects,
    };

    use super::{builds, checkout_row, facts, notice, pressure_facts, project_row, short, store};

    #[test]
    fn a_reading_states_its_time_of_day() {
        assert_eq!(super::observed("2026-11-20T23:04:46Z"), "23:04 UTC");
        assert_eq!(super::observed("unknown"), "unknown");
    }

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
            origin: None,
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
    fn a_row_states_its_size_its_build_state_and_its_standing() {
        let row = checkout_row(&fixture(), 6144, Some("/work"));
        assert_eq!(row.caption, "worktree · ~");
        assert_eq!(row.total, "3.0 KiB");
        assert_eq!(row.build, "2.0 KiB build");
        assert_eq!(row.modified, "2026-11-19");
        assert_eq!((row.state.as_str(), row.state_rank), ("landed", 2));
        // The bar measures against the largest project, and two thirds of it are build state.
        assert_eq!(row.share, 0.5);
        assert!((row.build_share - 2.0 / 3.0).abs() < 0.001);

        let mut held = fixture();
        held.hold.as_mut().unwrap().released = false;
        assert_eq!(checkout_row(&held, 6144, None).state, "in use by bosun");
        held.hold = None;
        held.linked = false;
        let row = checkout_row(&held, 6144, None);
        assert_eq!(row.state, "");
        assert_eq!(row.caption, "clone · /work");
    }

    #[test]
    fn a_project_row_counts_its_checkouts_and_those_in_use() {
        let mut held = fixture();
        held.hold.as_mut().unwrap().released = false;
        let checkouts = [held, fixture()];
        let found = projects(&checkouts, "", Sort::Size);
        let row = project_row(&found[0], true, Some("/work"));
        assert!(row.project && row.open);
        assert_eq!(row.name, "main");
        assert_eq!(row.caption, "2 checkouts · 1 in use · ~/main");
        assert_eq!(row.total, "6.0 KiB");

        // A second clone of one remote joins the project, which then takes the remote's name.
        let mut clone = fixture();
        clone.repository = Some("/old/main".into());
        let mut checkouts = [fixture(), clone];
        for checkout in &mut checkouts {
            checkout.origin = Some("github.com/acme/main".into());
        }
        let found = projects(&checkouts, "", Sort::Size);
        let row = project_row(&found[0], false, None);
        assert_eq!(row.name, "acme/main");
        assert_eq!(row.caption, "2 checkouts · 2 clones · github.com/acme/main");
    }

    #[test]
    fn a_frame_names_its_project_beyond_an_empty_one() {
        let mut empty = fixture();
        empty.path = "/work/aaa".into();
        empty.repository = None;
        (empty.own_bytes, empty.build_bytes) = (0, 0);
        let checkouts = [empty, fixture()];
        // The order of names puts the empty project first, and the map draws no frame for it.
        let found = projects(&checkouts, "", Sort::Name);
        assert_eq!(found[0].total_bytes(), 0);
        let drawn = map(&found, &checkouts, 400.0, 200.0);
        assert_eq!(drawn.frames.len(), 1);
        assert_eq!(super::frame(&drawn.frames[0], &found[1]).id, 1);
    }

    #[test]
    fn the_detail_names_the_repository_the_standing_and_each_build_directory() {
        let checkout = fixture();
        let found = facts(&checkout, Some("/work"));
        let labels: Vec<String> = found.iter().map(|f| f.label.to_string()).collect();
        assert_eq!(
            labels,
            [
                "Kind",
                "Repository",
                "Source",
                "Build state",
                "Last change",
                "Status"
            ]
        );
        assert_eq!(found[1].value, "~/main");
        assert_eq!(
            found[5].value,
            "bosun finished with it on 2026-11-18, and its work has landed. Suds can remove it once it is clean and idle."
        );
        let build = &builds(&checkout)[0];
        assert_eq!(build.path, "target");
        assert_eq!(build.size, "2.0 KiB");

        // The pane lists the largest build directory first.
        let mut many = fixture();
        many.build_state.push(BuildState {
            path: "/work/app/node_modules".into(),
            ecosystem: "node",
            bytes: 4096,
            newest_modified: "2026-11-20T00:00:00Z".into(),
        });
        let paths: Vec<String> = builds(&many).iter().map(|b| b.path.to_string()).collect();
        assert_eq!(paths, ["node_modules", "target"]);
    }

    #[test]
    fn the_pressure_states_a_usage_beyond_its_ceiling() {
        let card = PressureCard {
            level: Level::Hard,
            usage_bytes: 150 << 30,
            ceiling_bytes: 100 << 30,
            share: 1.0,
            filesystem_available_bytes: None,
            enforced: false,
            complete: true,
        };
        assert!(pressure_facts(&card).starts_with("150% of the ceiling"));
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
    fn a_sound_schedule_says_nothing_and_a_faulty_one_states_each_fault() {
        use suds_core::schedule::Standing;
        use suds_view::{FailedTrim, Upkeep};

        let sound = Upkeep {
            schedule: Standing::Current,
            last_run: Some("2026-11-21T06:39:16Z".into()),
            overdue: false,
            failed_trims: Vec::new(),
            journal_error: None,
        };
        assert_eq!(super::upkeep(&sound), "");

        let absent = Upkeep {
            schedule: Standing::Absent,
            last_run: None,
            ..sound.clone()
        };
        let line = super::upkeep(&absent);
        assert!(
            line.starts_with("No schedule runs the maintenance"),
            "{line}"
        );
        assert!(line.ends_with("suds schedule --install plans a new one, and --apply places it."));

        let trim = FailedTrim {
            store: "pnpm.store".into(),
            reason: "could not run pnpm".into(),
        };
        let faulty = Upkeep {
            schedule: Standing::Stale("the unit states no search path".into()),
            overdue: true,
            failed_trims: vec![trim],
            ..sound.clone()
        };
        let text = super::upkeep(&faulty);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("The maintenance schedule is stale: the unit states no"));
        assert!(lines[0].ends_with("and --apply places it."));
        assert_eq!(
            lines[1],
            "The scheduled maintenance last ran on 2026-11-21, more than a day ago."
        );
        assert_eq!(
            lines[2],
            "The last scheduled run could not trim pnpm.store: could not run pnpm."
        );
        let never = Upkeep {
            last_run: None,
            ..faulty
        };
        assert!(super::upkeep(&never).contains("has not run yet"));

        let unread = Upkeep {
            schedule: Standing::Unreadable("could not read /u: permission denied".into()),
            journal_error: Some("the journal is corrupt".into()),
            ..sound
        };
        let text = super::upkeep(&unread);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("Suds could not read the maintenance schedule: could not"));
        assert!(lines[1].ends_with("is unknown: the journal is corrupt"));
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
