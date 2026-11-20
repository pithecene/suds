//! The projects of some checkouts and the storage map that draws them.

use suds_view::{Checkout, Hold, Rect, Sort, map, projects, squarify};

fn checkout(path: &str, repository: Option<&str>, own: u64, build: u64, day: &str) -> Checkout {
    let (parent, name) = path.rsplit_once('/').unwrap();
    Checkout {
        path: path.into(),
        name: name.into(),
        parent: parent.into(),
        linked: repository.is_some(),
        repository: repository.map(str::to_owned),
        origin: None,
        own_bytes: own,
        build_bytes: build,
        build_state: Vec::new(),
        newest_modified: format!("2026-11-{day}T00:00:00Z"),
        hold: None,
        share: 0.0,
    }
}

fn held(mut checkout: Checkout, released: bool) -> Checkout {
    checkout.hold = Some(Hold {
        client: "bosun".into(),
        since: "2026-11-01T00:00:00Z".into(),
        released,
        landed: None,
    });
    checkout
}

/// The repository `app` with its linked checkouts, and the repository `tool` alone.
fn fixture() -> Vec<Checkout> {
    vec![
        checkout("/work/app", None, 40, 0, "01"),
        held(
            checkout("/work/app/.trees/one", Some("/work/app"), 10, 30, "20"),
            false,
        ),
        held(
            checkout("/work/app/.trees/two", Some("/work/app"), 10, 0, "05"),
            true,
        ),
        checkout("/work/tool", None, 60, 0, "25"),
    ]
}

#[test]
fn a_linked_checkout_joins_the_project_of_its_repository() {
    let checkouts = fixture();
    let found = projects(&checkouts, "", Sort::Size);

    let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["app", "tool"]);
    let app = &found[0];
    assert_eq!(
        (app.key.as_str(), app.remote, app.clones),
        ("/work/app", false, 1)
    );
    // The members stand in order of size: the checkouts of 40, 40, and 10 bytes.
    assert_eq!(app.members, [0, 1, 2]);
    assert_eq!((app.own_bytes, app.build_bytes), (60, 30));
    assert_eq!(app.newest_modified, "2026-11-20T00:00:00Z");
    // A released checkout counts as held by no client.
    assert_eq!(app.held, 1);
    assert_eq!(app.share, 1.0);
    assert!((found[1].share - 60.0 / 90.0).abs() < 0.001);
}

#[test]
fn the_sort_orders_the_projects_and_a_query_keeps_only_its_checkouts() {
    let checkouts = fixture();
    let order = |sort| -> Vec<String> {
        projects(&checkouts, "", sort)
            .into_iter()
            .map(|project| project.name)
            .collect()
    };
    assert_eq!(order(Sort::Name), ["app", "tool"]);
    assert_eq!(order(Sort::Modified), ["tool", "app"]);
    assert_eq!(order(Sort::BuildState), ["app", "tool"]);

    let found = projects(&checkouts, "TREES/two", Sort::Size);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].members, [2]);
    // The sums cover the kept checkouts alone, so `tool` would now lead.
    assert_eq!(found[0].total_bytes(), 10);
}

#[test]
fn the_clones_of_one_remote_share_a_project() {
    let mut checkouts = fixture();
    // A second clone of the remote stands in another directory, with a checkout linked to it.
    checkouts.push(checkout("/old/app", None, 5, 0, "02"));
    checkouts.push(checkout("/old/app-tree", Some("/old/app"), 5, 0, "03"));
    for position in [0, 1, 2, 4, 5] {
        checkouts[position].origin = Some("github.com/acme/app".into());
    }
    let found = projects(&checkouts, "", Sort::Size);

    assert_eq!(found.len(), 2);
    let app = &found[0];
    assert_eq!(app.name, "acme/app");
    assert!(app.remote);
    assert_eq!(app.clones, 2);
    assert_eq!(app.members.len(), 5);
    assert_eq!(found[1].name, "tool");
}

fn inside(inner: &Rect, outer: &Rect) -> bool {
    let slack = 0.01;
    inner.x >= outer.x - slack
        && inner.y >= outer.y - slack
        && inner.x + inner.width <= outer.x + outer.width + slack
        && inner.y + inner.height <= outer.y + outer.height + slack
}

#[test]
fn each_rectangle_takes_its_weight_of_the_bounds() {
    let bounds = Rect {
        x: 10.0,
        y: 20.0,
        width: 600.0,
        height: 400.0,
    };
    let weights = [6, 6, 4, 3, 2, 2, 1, 0];
    let rects = squarify(&weights, bounds);

    assert_eq!(rects.len(), weights.len());
    let total: u64 = weights.iter().sum();
    for (rect, weight) in rects.iter().zip(weights) {
        assert!(inside(rect, &bounds), "{rect:?}");
        let expected = bounds.area() * weight as f32 / total as f32;
        assert!((rect.area() - expected).abs() < 1.0, "{rect:?} {expected}");
    }
    // The paper's own example keeps every rectangle within a ratio of three.
    for rect in &rects[..7] {
        let ratio = (rect.width / rect.height).max(rect.height / rect.width);
        assert!(ratio < 3.0, "{rect:?}");
    }
    assert!(squarify(&[0, 0], bounds).iter().all(|r| r.area() == 0.0));
    assert!(squarify(&[], bounds).is_empty());
}

#[test]
fn the_map_draws_each_checkout_inside_the_frame_of_its_project() {
    let checkouts = fixture();
    let found = projects(&checkouts, "", Sort::Size);
    let drawn = map(&found, &checkouts, 600.0, 300.0);

    assert_eq!(drawn.frames.len(), 2);
    assert_eq!(drawn.tiles.len(), 4);
    assert!(drawn.frames.iter().all(|frame| frame.labelled));
    for tile in &drawn.tiles {
        let frame = &drawn.frames[tile.project];
        assert!(inside(&tile.rect, &frame.rect), "{tile:?}");
        // The project's name stands above its tiles.
        assert!(tile.rect.y >= frame.rect.y + suds_view::LABEL - 0.01);
        assert!(inside(&tile.build, &tile.rect));
    }
    // Three quarters of the first linked checkout are build state.
    let one = drawn.tiles.iter().find(|tile| tile.checkout == 1).unwrap();
    assert!((one.build.area() / one.rect.area() - 0.75).abs() < 0.001);
    let main = drawn.tiles.iter().find(|tile| tile.checkout == 0).unwrap();
    assert_eq!(main.build.area(), 0.0);
}

#[test]
fn a_frame_without_room_stands_for_its_whole_project() {
    let mut checkouts = fixture();
    // The project `tool` now takes a thousandth of the map.
    checkouts[0].own_bytes = 60_000;
    let found = projects(&checkouts, "", Sort::Size);
    let drawn = map(&found, &checkouts, 600.0, 300.0);

    let tool = drawn
        .frames
        .iter()
        .find(|f| found[f.project].name == "tool");
    let tool = tool.unwrap();
    assert!(!tool.detailed && !tool.labelled);
    assert!(drawn.tiles.iter().all(|tile| tile.project != tool.project));
}
