//! The checkouts of one repository as one project.

use std::collections::HashMap;
use std::path::Path;

use crate::workstation::{Checkout, Sort, arrange};

/// One repository's checkouts: its main checkout and each linked checkout.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    /// The path of the repository's main checkout.
    pub path: String,
    pub name: String,
    /// The positions of the project's checkouts in the view's checkouts, in the order of the sort.
    pub members: Vec<usize>,
    pub own_bytes: u64,
    pub build_bytes: u64,
    /// The newest change among the project's checkouts.
    pub newest_modified: String,
    /// The number of checkouts that a client holds and has not released.
    pub held: usize,
    /// The project's size as a share of the largest project's, from 0 to 1.
    pub share: f32,
}

impl Project {
    pub fn total_bytes(&self) -> u64 {
        self.own_bytes.saturating_add(self.build_bytes)
    }
}

/// The path that names a checkout's project: its repository's main checkout, or itself.
fn key(checkout: &Checkout) -> &str {
    checkout.repository.as_deref().unwrap_or(&checkout.path)
}

/// The projects of each checkout whose path holds `query`, in the order of
/// `sort`. The sort orders the projects by their sums and each project's
/// checkouts by their own figures.
pub fn projects(checkouts: &[Checkout], query: &str, sort: Sort) -> Vec<Project> {
    let mut found: HashMap<&str, usize> = HashMap::new();
    let mut projects: Vec<Project> = Vec::new();
    for position in arrange(checkouts, query, sort) {
        let checkout = &checkouts[position];
        let path = key(checkout);
        let at = *found.entry(path).or_insert_with(|| {
            let name = Path::new(path).file_name();
            projects.push(Project {
                path: path.to_owned(),
                name: name.map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned()),
                members: Vec::new(),
                own_bytes: 0,
                build_bytes: 0,
                newest_modified: String::new(),
                held: 0,
                share: 0.0,
            });
            projects.len() - 1
        });
        let project = &mut projects[at];
        project.members.push(position);
        project.own_bytes = project.own_bytes.saturating_add(checkout.own_bytes);
        project.build_bytes = project.build_bytes.saturating_add(checkout.build_bytes);
        if checkout.newest_modified > project.newest_modified {
            project.newest_modified.clone_from(&checkout.newest_modified);
        }
        if checkout.hold.as_ref().is_some_and(|hold| !hold.released) {
            project.held += 1;
        }
    }
    let by_path = |a: &Project, b: &Project| a.path.cmp(&b.path);
    match sort {
        Sort::Size => projects.sort_by(|a, b| {
            b.total_bytes()
                .cmp(&a.total_bytes())
                .then_with(|| by_path(a, b))
        }),
        Sort::Name => projects.sort_by(|a, b| {
            let name = |project: &Project| project.name.to_ascii_lowercase();
            name(a).cmp(&name(b)).then_with(|| by_path(a, b))
        }),
        Sort::Modified => projects.sort_by(|a, b| {
            b.newest_modified
                .cmp(&a.newest_modified)
                .then_with(|| by_path(a, b))
        }),
        Sort::BuildState => projects.sort_by(|a, b| {
            b.build_bytes
                .cmp(&a.build_bytes)
                .then_with(|| by_path(a, b))
        }),
    }
    let largest = projects.iter().map(Project::total_bytes).max().unwrap_or(0);
    for project in &mut projects {
        project.share = if largest == 0 {
            0.0
        } else {
            (project.total_bytes() as f64 / largest as f64) as f32
        };
    }
    projects
}
