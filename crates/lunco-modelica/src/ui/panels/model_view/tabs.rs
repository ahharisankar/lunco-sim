//! Tab table and lifecycle logic for Modelica model views.

use std::collections::{HashMap, HashSet};
use bevy::prelude::*;
use lunco_doc::DocumentId;
use super::types::{ModelTabState, ModelViewMode, TabId};

/// Registry of open model view tabs.
#[derive(Resource, Default)]
pub struct ModelTabs {
    pub(super) tabs: HashMap<TabId, ModelTabState>,
    next_id: u64,
    preview_slot: Option<TabId>,
}

impl ModelTabs {
    fn allocate_id(&mut self) -> TabId {
        self.next_id = self.next_id.saturating_add(1);
        self.next_id
    }

    pub fn ensure_for(
        &mut self,
        doc: DocumentId,
        drilled_class: Option<String>,
    ) -> TabId {
        if let Some((id, _)) = self.tabs.iter().find(|(_, s)| {
            s.doc == doc && s.drilled_class.as_deref() == drilled_class.as_deref()
        }) {
            return *id;
        }
        let id = self.allocate_id();
        self.tabs.insert(
            id,
            ModelTabState {
                doc,
                drilled_class,
                view_mode: ModelViewMode::default(),
                pinned: true,
                load_error: None,
            },
        );
        id
    }

    pub fn ensure_preview_for(
        &mut self,
        doc: DocumentId,
        drilled_class: Option<String>,
    ) -> (TabId, Option<TabId>) {
        if let Some((id, _)) = self.tabs.iter().find(|(_, s)| {
            s.doc == doc && s.drilled_class.as_deref() == drilled_class.as_deref()
        }) {
            return (*id, None);
        }
        let id = self.allocate_id();
        self.tabs.insert(
            id,
            ModelTabState {
                doc,
                drilled_class,
                view_mode: ModelViewMode::default(),
                pinned: false,
                load_error: None,
            },
        );
        let evict = self.preview_slot.replace(id);
        (id, evict)
    }

    pub fn open_new(
        &mut self,
        doc: DocumentId,
        drilled_class: Option<String>,
    ) -> TabId {
        let id = self.allocate_id();
        self.tabs.insert(
            id,
            ModelTabState {
                doc,
                drilled_class,
                view_mode: ModelViewMode::default(),
                pinned: true,
                load_error: None,
            },
        );
        id
    }

    pub fn pin(&mut self, tab_id: TabId) {
        if let Some(state) = self.tabs.get_mut(&tab_id) {
            state.pinned = true;
        }
        if self.preview_slot == Some(tab_id) {
            self.preview_slot = None;
        }
    }

    pub fn pin_all_for_doc(&mut self, doc: DocumentId) {
        let mut clear_preview = false;
        for (id, state) in self.tabs.iter_mut() {
            if state.doc == doc {
                state.pinned = true;
                if self.preview_slot == Some(*id) {
                    clear_preview = true;
                }
            }
        }
        if clear_preview {
            self.preview_slot = None;
        }
    }

    pub fn close_tab(&mut self, tab_id: TabId) -> Option<ModelTabState> {
        if self.preview_slot == Some(tab_id) {
            self.preview_slot = None;
        }
        self.tabs.remove(&tab_id)
    }

    pub fn iter_mut_for_doc(
        &mut self,
        doc: DocumentId,
    ) -> impl Iterator<Item = (TabId, &mut ModelTabState)> + '_ {
        self.tabs
            .iter_mut()
            .filter(move |(_, s)| s.doc == doc)
            .map(|(id, s)| (*id, s))
    }

    pub fn drilled_class_for_doc(&self, doc: DocumentId) -> Option<String> {
        let tab_id = self.any_for_doc(doc)?;
        self.get(tab_id)?.drilled_class.clone()
    }

    pub fn close_drilled_into(&mut self, doc: DocumentId, qualified: &str) -> Vec<TabId> {
        if qualified.is_empty() {
            return Vec::new();
        }
        let prefix = format!("{qualified}.");
        let to_close: Vec<TabId> = self
            .tabs
            .iter()
            .filter_map(|(id, s)| {
                if s.doc != doc {
                    return None;
                }
                let drilled = s.drilled_class.as_deref()?;
                (drilled == qualified || drilled.starts_with(&prefix)).then_some(*id)
            })
            .collect();
        for id in &to_close {
            self.tabs.remove(id);
        }
        to_close
    }

    pub fn close_all_for_doc(&mut self, doc: DocumentId) -> Vec<TabId> {
        let ids: Vec<TabId> = self
            .tabs
            .iter()
            .filter_map(|(id, s)| (s.doc == doc).then_some(*id))
            .collect();
        for id in &ids {
            self.tabs.remove(id);
        }
        ids
    }

    pub fn close(&mut self, doc: DocumentId) {
        let _ = self.close_all_for_doc(doc);
    }

    pub fn get(&self, tab_id: TabId) -> Option<&ModelTabState> {
        self.tabs.get(&tab_id)
    }

    pub fn get_mut(&mut self, tab_id: TabId) -> Option<&mut ModelTabState> {
        self.tabs.get_mut(&tab_id)
    }

    pub fn any_for_doc(&self, doc: DocumentId) -> Option<TabId> {
        self.tabs
            .iter()
            .find_map(|(id, s)| (s.doc == doc).then_some(*id))
    }

    pub fn find_for(
        &self,
        doc: DocumentId,
        drilled_class: Option<&str>,
    ) -> Option<TabId> {
        self.tabs.iter().find_map(|(id, s)| {
            (s.doc == doc && s.drilled_class.as_deref() == drilled_class).then_some(*id)
        })
    }

    pub fn find_for_mut(
        &mut self,
        doc: DocumentId,
        drilled_class: Option<&str>,
    ) -> Option<&mut ModelTabState> {
        self.tabs.iter_mut().find_map(|(_, s)| {
            (s.doc == doc && s.drilled_class.as_deref() == drilled_class).then_some(s)
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = (TabId, &ModelTabState)> + '_ {
        self.tabs.iter().map(|(id, s)| (*id, s))
    }

    pub fn iter_docs(&self) -> impl Iterator<Item = DocumentId> + '_ {
        let mut seen = HashSet::new();
        self.tabs
            .values()
            .filter_map(move |s| seen.insert(s.doc).then_some(s.doc))
    }

    pub fn contains(&self, doc: DocumentId) -> bool {
        self.any_for_doc(doc).is_some()
    }

    pub fn count_for_doc(&self, doc: DocumentId) -> usize {
        self.tabs.values().filter(|s| s.doc == doc).count()
    }
}
