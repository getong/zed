use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};

use async_recursion::async_recursion;
use gpui::{
    geometry::{rect::RectF, vector::vec2f},
    AsyncAppContext, Axis, ModelHandle, Task, ViewHandle, WindowBounds,
};

use db::sqlez::{
    bindable::{Bind, Column, StaticColumnCount},
    statement::Statement,
};
use project::Project;
use serde::{Deserialize, Serialize};
use settings::DockAnchor;
use store::Record;
use util::ResultExt;
use uuid::Uuid;

use crate::{
    dock::DockPosition, ItemDeserializers, Member, Pane, PaneAxis, Workspace, WorkspaceId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceLocation(Arc<Vec<PathBuf>>);

impl WorkspaceLocation {
    pub fn paths(&self) -> Arc<Vec<PathBuf>> {
        self.0.clone()
    }
}

impl<P: AsRef<Path>, T: IntoIterator<Item = P>> From<T> for WorkspaceLocation {
    fn from(iterator: T) -> Self {
        let mut roots = iterator
            .into_iter()
            .map(|p| p.as_ref().to_path_buf())
            .collect::<Vec<_>>();
        roots.sort();
        Self(Arc::new(roots))
    }
}

impl StaticColumnCount for WorkspaceLocation {}
impl Bind for &WorkspaceLocation {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        bincode::serialize(&self.0)
            .expect("Bincode serialization of paths should not fail")
            .bind(statement, start_index)
    }
}

impl Column for WorkspaceLocation {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let blob = statement.column_blob(start_index)?;
        Ok((
            WorkspaceLocation(bincode::deserialize(blob).context("Bincode failed")?),
            start_index + 1,
        ))
    }
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub id: WorkspaceId,
    pub location: WorkspaceLocation,
    pub dock_position: DockPosition,
    pub center_group: PaneGroupState,
    pub dock_pane: PaneState,
    pub left_sidebar_open: bool,
    pub bounds: Option<WindowBoundsState>,
    pub display: Option<Uuid>,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WindowBoundsState {
    Fullscreen,
    Maximized,
    Fixed {
        origin_x: f32,
        origin_y: f32,
        width: f32,
        height: f32,
    },
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub enum PaneGroupState {
    Group {
        axis: Axis,
        children: Vec<PaneGroupState>,
    },
    Pane(PaneState),
}

impl Record for WorkspaceState {
    fn namespace() -> &'static str {
        "Workspace"
    }

    fn schema_version() -> u64 {
        0
    }

    fn serialize(&self) -> Vec<u8> {
        todo!()
    }

    fn deserialize(version: u64, data: Vec<u8>) -> Result<Self>
    where
        Self: Sized,
    {
        todo!()
    }
}

impl WindowBoundsState {
    pub fn from_window_bounds(bounds: WindowBounds) -> Self {
        match bounds {
            WindowBounds::Fullscreen => Self::Fullscreen,
            WindowBounds::Maximized => Self::Maximized,
            WindowBounds::Fixed(bounds) => Self::Fixed {
                origin_x: bounds.origin_x(),
                origin_y: bounds.origin_y(),
                width: bounds.width(),
                height: bounds.height(),
            },
        }
    }

    pub fn to_window_bounds(&self) -> WindowBounds {
        match self {
            WindowBoundsState::Fullscreen => WindowBounds::Fullscreen,
            WindowBoundsState::Maximized => WindowBounds::Maximized,
            WindowBoundsState::Fixed {
                origin_x,
                origin_y,
                width,
                height,
            } => WindowBounds::Fixed(RectF::new(
                vec2f(*origin_x, *origin_y),
                vec2f(*width, *height),
            )),
        }
    }
}

#[cfg(test)]
impl Default for PaneGroupState {
    fn default() -> Self {
        Self::Pane(PaneState {
            children: vec![SerializedItem::default()],
            active: false,
        })
    }
}

impl PaneGroupState {
    #[async_recursion(?Send)]
    pub(crate) async fn deserialize(
        &self,
        project: &ModelHandle<Project>,
        workspace_id: WorkspaceId,
        workspace: &ViewHandle<Workspace>,
        cx: &mut AsyncAppContext,
    ) -> Option<(Member, Option<ViewHandle<Pane>>)> {
        match self {
            PaneGroupState::Group { axis, children } => {
                let mut current_active_pane = None;
                let mut members = Vec::new();
                for child in children {
                    if let Some((new_member, active_pane)) = child
                        .deserialize(project, workspace_id, workspace, cx)
                        .await
                    {
                        members.push(new_member);
                        current_active_pane = current_active_pane.or(active_pane);
                    }
                }

                if members.is_empty() {
                    return None;
                }

                if members.len() == 1 {
                    return Some((members.remove(0), current_active_pane));
                }

                Some((
                    Member::Axis(PaneAxis {
                        axis: *axis,
                        members,
                    }),
                    current_active_pane,
                ))
            }
            PaneGroupState::Pane(serialized_pane) => {
                let pane = workspace.update(cx, |workspace, cx| workspace.add_pane(cx));
                let active = serialized_pane.active;
                serialized_pane
                    .deserialize_to(project, &pane, workspace_id, workspace, cx)
                    .await;

                if pane.read_with(cx, |pane, _| pane.items_len() != 0) {
                    Some((Member::Pane(pane.clone()), active.then(|| pane)))
                } else {
                    workspace.update(cx, |workspace, cx| workspace.remove_pane(pane, cx));
                    None
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, Default, Clone, Serialize, Deserialize)]
pub struct PaneState {
    pub(crate) active: bool,
    pub(crate) children: Vec<SerializedItem>,
}

impl PaneState {
    pub fn new(children: Vec<SerializedItem>, active: bool) -> Self {
        PaneState { children, active }
    }

    pub async fn deserialize_to(
        &self,
        project: &ModelHandle<Project>,
        pane_handle: &ViewHandle<Pane>,
        workspace_id: WorkspaceId,
        workspace: &ViewHandle<Workspace>,
        cx: &mut AsyncAppContext,
    ) {
        let mut active_item_index = None;
        for (index, item) in self.children.iter().enumerate() {
            let project = project.clone();
            let item_handle = pane_handle
                .update(cx, |_, cx| {
                    if let Some(deserializer) = cx.global::<ItemDeserializers>().get(&item.kind) {
                        deserializer(
                            project,
                            workspace.downgrade(),
                            workspace_id,
                            item.item_id,
                            cx,
                        )
                    } else {
                        Task::ready(Err(anyhow::anyhow!(
                            "Deserializer does not exist for item kind: {}",
                            item.kind
                        )))
                    }
                })
                .await
                .log_err();

            if let Some(item_handle) = item_handle {
                workspace.update(cx, |workspace, cx| {
                    Pane::add_item(workspace, &pane_handle, item_handle, false, false, None, cx);
                })
            }

            if item.active {
                active_item_index = Some(index);
            }
        }

        if let Some(active_item_index) = active_item_index {
            pane_handle.update(cx, |pane, cx| {
                pane.activate_item(active_item_index, false, false, cx);
            })
        }
    }
}

pub type GroupId = i64;
pub type PaneId = i64;
pub type ItemId = usize;

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct SerializedItem {
    pub kind: Arc<str>,
    pub item_id: ItemId,
    pub active: bool,
}

impl SerializedItem {
    pub fn new(kind: impl AsRef<str>, item_id: ItemId, active: bool) -> Self {
        Self {
            kind: Arc::from(kind.as_ref()),
            item_id,
            active,
        }
    }
}

#[cfg(test)]
impl Default for SerializedItem {
    fn default() -> Self {
        SerializedItem {
            kind: Arc::from("Terminal"),
            item_id: 100000,
            active: false,
        }
    }
}

impl StaticColumnCount for SerializedItem {
    fn column_count() -> usize {
        3
    }
}
impl Bind for &SerializedItem {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        let next_index = statement.bind(self.kind.clone(), start_index)?;
        let next_index = statement.bind(self.item_id, next_index)?;
        statement.bind(self.active, next_index)
    }
}

impl Column for SerializedItem {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (kind, next_index) = Arc::<str>::column(statement, start_index)?;
        let (item_id, next_index) = ItemId::column(statement, next_index)?;
        let (active, next_index) = bool::column(statement, next_index)?;
        Ok((
            SerializedItem {
                kind,
                item_id,
                active,
            },
            next_index,
        ))
    }
}

impl StaticColumnCount for DockPosition {
    fn column_count() -> usize {
        2
    }
}
impl Bind for DockPosition {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        let next_index = statement.bind(self.is_visible(), start_index)?;
        statement.bind(self.anchor(), next_index)
    }
}

impl Column for DockPosition {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (visible, next_index) = bool::column(statement, start_index)?;
        let (dock_anchor, next_index) = DockAnchor::column(statement, next_index)?;
        let position = if visible {
            DockPosition::Shown(dock_anchor)
        } else {
            DockPosition::Hidden(dock_anchor)
        };
        Ok((position, next_index))
    }
}

#[cfg(test)]
mod tests {
    use db::sqlez::connection::Connection;
    use settings::DockAnchor;

    use super::WorkspaceLocation;

    #[test]
    fn test_workspace_round_trips() {
        let db = Connection::open_memory(Some("workspace_id_round_trips"));

        db.exec(indoc::indoc! {"
                CREATE TABLE workspace_id_test(
                    workspace_id INTEGER,
                    dock_anchor TEXT
                );"})
            .unwrap()()
        .unwrap();

        let workspace_id: WorkspaceLocation = WorkspaceLocation::from(&["\test2", "\test1"]);

        db.exec_bound("INSERT INTO workspace_id_test(workspace_id, dock_anchor) VALUES (?,?)")
            .unwrap()((&workspace_id, DockAnchor::Bottom))
        .unwrap();

        assert_eq!(
            db.select_row("SELECT workspace_id, dock_anchor FROM workspace_id_test LIMIT 1")
                .unwrap()()
            .unwrap(),
            Some((
                WorkspaceLocation::from(&["\test1", "\test2"]),
                DockAnchor::Bottom
            ))
        );
    }
}
