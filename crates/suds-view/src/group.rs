//! The stores of one view in groups, so a long list reads as a short one.

use crate::workstation::{Store, StoreStanding};

/// What the stores of one group share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    /// The language whose files the stores hold.
    #[default]
    Ecosystem,
    /// The tool that owns the stores, a layer beneath the ecosystem.
    Tool,
    /// The class of storage: a shared store, a toolchain, or a machine.
    Class,
}

/// The stores that share one ecosystem, one tool, or one class.
#[derive(Debug, Clone, PartialEq)]
pub struct StoreGroup {
    pub label: String,
    pub bytes: u64,
    /// The group's size as a share of the largest group's.
    pub share: f32,
    /// The count of the stores over their budgets.
    pub over: usize,
    /// The group's stores, in the order of the view.
    pub stores: Vec<Store>,
}

impl StoreGroup {
    /// Whether the workstation holds none of the group's stores.
    pub fn absent(&self) -> bool {
        let absent = |store: &Store| store.standing == StoreStanding::Absent;
        self.stores.iter().all(absent)
    }
}

/// Groups `stores` by `grouping`, the largest group first. Two groups of one
/// size stand in the order of their labels.
pub fn store_groups(stores: &[Store], grouping: Grouping) -> Vec<StoreGroup> {
    let mut groups: Vec<StoreGroup> = Vec::new();
    for store in stores {
        let label = match grouping {
            Grouping::Ecosystem => store.ecosystem,
            Grouping::Tool => store.tool,
            Grouping::Class => store.class,
        };
        let over = usize::from(store.standing == StoreStanding::Over);
        match groups.iter_mut().find(|group| group.label == label) {
            Some(group) => {
                group.bytes = group.bytes.saturating_add(store.bytes);
                group.over += over;
                group.stores.push(store.clone());
            }
            None => groups.push(StoreGroup {
                label: label.to_owned(),
                bytes: store.bytes,
                share: 0.0,
                over,
                stores: vec![store.clone()],
            }),
        }
    }
    groups.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)));
    let largest = groups.first().map_or(0, |group| group.bytes);
    for group in &mut groups {
        group.share = if largest == 0 {
            0.0
        } else {
            group.bytes as f32 / largest as f32
        };
    }
    groups
}
