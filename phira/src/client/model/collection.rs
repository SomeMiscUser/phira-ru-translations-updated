use std::{
    borrow::Cow,
    collections::HashSet,
    hash::{Hash, Hasher},
};

use crate::{
    client::{recv_raw, Client, File},
    dir, get_data,
    page::{local_illustration, Illustration},
};

use super::{Chart, Object, Ptr, User};
use anyhow::Result;
use chrono::{DateTime, Utc};
use prpr::{ext::BLACK_TEXTURE, info::ChartInfo, task::Task, ui::Dialog};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct Collection {
    pub id: i32,
    pub cover: Option<File>,
    pub owner: Ptr<User>,
    pub name: String,
    pub description: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub charts: Vec<Chart>,
    pub public: bool,
}
impl Object for Collection {
    const QUERY_PATH: &'static str = "collection";

    fn id(&self) -> i32 {
        self.id
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChartRef {
    Online(i32, Option<Box<Chart>>),
    Local(String),
}
impl ChartRef {
    pub fn local_path(&self) -> Cow<'_, str> {
        match self {
            Self::Online(id, _) => Cow::Owned(format!("download/{id}")),
            Self::Local(path) => Cow::Borrowed(path),
        }
    }

    pub fn is_online(&self) -> bool {
        self.local_path().starts_with("download/")
    }

    pub fn id(&self) -> Option<i32> {
        match self {
            Self::Online(id, _) => Some(*id),
            Self::Local(_) => None,
        }
    }
}

impl From<Chart> for ChartRef {
    fn from(chart: Chart) -> Self {
        ChartRef::Online(chart.id, Some(Box::new(chart)))
    }
}

impl PartialEq for ChartRef {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ChartRef::Online(id1, _), ChartRef::Online(id2, _)) => id1 == id2,
            (ChartRef::Local(path1), ChartRef::Local(path2)) => path1 == path2,
            _ => false,
        }
    }
}
impl Eq for ChartRef {}

impl Hash for ChartRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            ChartRef::Online(id, _) => id.hash(state),
            ChartRef::Local(path) => path.hash(state),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub enum CollectionCover {
    Unset,
    Online(File),
    LocalChart(String),
}

pub enum CollectionUpdate {
    Unchanged,
    Updated {
        sync_task: Option<Task<Result<(Collection, bool)>>>,
        add: bool,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LocalCollection {
    pub id: Option<i32>,
    pub owner: Option<Ptr<User>>,
    pub cover: CollectionCover,
    pub name: String,
    pub description: String,
    pub remote_updated: Option<DateTime<Utc>>,
    pub charts: Vec<ChartRef>,
    #[serde(default)]
    pub public: bool,
    pub is_default: bool,
}
impl LocalCollection {
    pub fn new(name: String) -> Self {
        Self {
            id: None,
            owner: None,
            cover: CollectionCover::Unset,
            name,
            description: String::new(),
            remote_updated: None,
            charts: Vec::new(),
            public: false,
            is_default: false,
        }
    }

    pub fn cover(&self) -> Illustration {
        let mut cover = self.cover.clone();
        if matches!(cover, CollectionCover::Unset) {
            cover = match self.charts.first() {
                None => CollectionCover::Unset,
                Some(ChartRef::Online(_, chart)) => CollectionCover::Online(chart.as_ref().unwrap().illustration.clone()),
                Some(ChartRef::Local(path)) => CollectionCover::LocalChart(path.clone()),
            };
        }
        match cover {
            CollectionCover::Unset => Illustration::from_done(BLACK_TEXTURE.clone()),
            CollectionCover::Online(file) => Illustration::from_file_thumbnail(file),
            CollectionCover::LocalChart(path) => local_illustration(path, BLACK_TEXTURE.clone(), false),
        }
    }

    pub fn is_owned(&self) -> bool {
        self.id.is_none()
            || self
                .owner
                .as_ref()
                .is_some_and(|it| get_data().me.as_ref().is_some_and(|me| me.id == it.id))
    }

    pub fn merge(&self, col: &Collection) -> Self {
        assert_eq!(self.id, Some(col.id));
        Self {
            id: Some(col.id),
            owner: Some(col.owner.clone()),
            cover: match &col.cover {
                None => CollectionCover::Unset,
                Some(file) => CollectionCover::Online(file.clone()),
            },
            name: col.name.clone(),
            description: self.description.clone(),
            remote_updated: Some(col.updated),
            charts: col.charts.iter().cloned().map(Into::into).collect(),
            public: col.public,
            is_default: self.is_default,
        }
    }

    #[must_use]
    pub fn update(mut self, uuid: Uuid, charts: &[ChartRef], add: bool) -> CollectionUpdate {
        let data = get_data();
        if self.id.is_some() && charts.iter().any(|it| !it.is_online()) {
            let dir = dir::charts().unwrap();
            let charts: Vec<_> = charts
                .iter()
                .filter(|it| !it.is_online())
                .filter_map(|it| {
                    let path = format!("{dir}/{}/info.yml", it.local_path());
                    let info = std::fs::read_to_string(path).ok()?;
                    serde_yaml::from_str::<ChartInfo>(&info).ok().map(|info| info.name)
                })
                .collect();
            Dialog::simple(ttl!("favorites-online-only", "charts" => charts.join(", "))).show();
            return CollectionUpdate::Unchanged;
        }

        let should_upload = self.id.is_some() && !get_data().config.offline_mode;
        let mut updated = false;
        if add {
            let local_paths: HashSet<String> = self.charts.iter().map(|it| it.local_path().into_owned()).collect();
            for chart in charts {
                if !local_paths.contains(chart.local_path().as_ref()) {
                    self.charts.push(chart.clone());
                    updated = true;
                }
            }
        } else {
            let to_remove: HashSet<ChartRef> = charts.iter().cloned().collect();
            self.charts.retain(|it| {
                if to_remove.contains(it) {
                    updated = true;
                    false
                } else {
                    true
                }
            });
        }
        if !updated {
            return CollectionUpdate::Unchanged;
        }

        let id = self.id;
        data.set_collection_info(&uuid, self).unwrap();
        if !should_upload {
            return CollectionUpdate::Updated { sync_task: None, add };
        }

        let col_ids = charts.iter().filter_map(|it| it.id()).collect::<Vec<_>>();
        CollectionUpdate::Updated {
            sync_task: Some(Task::new(async move {
                let resp: Collection =
                    recv_raw(Client::request(Method::PATCH, format!("/collection/{}", id.unwrap())).json(&CollectionPatch::Set(col_ids)))
                        .await?
                        .json()
                        .await?;
                Ok((resp, add))
            })),
            add,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CollectionPatch {
    Set(Vec<i32>),
    Public(bool),
    Cover(i32),
}

#[derive(Serialize)]
pub struct CollectionContent {
    pub name: String,
    pub description: String,
    pub charts: Vec<i32>,
    pub public: bool,
}
