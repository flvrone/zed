use std::{
    cmp,
    ops::Range,
    path::{Path, PathBuf},
};

use crate::{editor_settings, Anchor, Editor, ExcerptId, MultiBuffer};
use anyhow::Context;
use clock::{Global, Local};
use gpui::{ModelHandle, Task, ViewContext};
use log::error;
use project::{InlayHint, InlayHintKind};
use util::post_inc;

use collections::{hash_map, BTreeMap, HashMap, HashSet};

#[derive(Debug, Copy, Clone)]
pub enum InlayRefreshReason {
    Settings(editor_settings::InlayHints),
    Regular,
}

#[derive(Debug, Clone, Default)]
pub struct InlayCache {
    inlays_per_buffer: HashMap<PathBuf, BufferInlays>,
    allowed_hint_kinds: HashSet<Option<InlayHintKind>>,
    next_inlay_id: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AnchorKey {
    offset: usize,
    version: Local,
}

#[derive(Clone, Debug)]
pub struct OrderedByAnchorOffset<T>(pub BTreeMap<AnchorKey, (Anchor, T)>);

impl<T> OrderedByAnchorOffset<T> {
    pub fn add(&mut self, anchor: Anchor, t: T) {
        let key = AnchorKey {
            offset: anchor.text_anchor.offset,
            version: anchor.text_anchor.timestamp,
        };
        self.0.insert(key, (anchor, t));
    }

    fn into_ordered_elements(self) -> impl Iterator<Item = (Anchor, T)> {
        self.0.into_values()
    }

    fn ordered_elements(&self) -> impl Iterator<Item = &(Anchor, T)> {
        self.0.values()
    }
}

impl<T> Default for OrderedByAnchorOffset<T> {
    fn default() -> Self {
        Self(BTreeMap::default())
    }
}

#[derive(Clone, Debug, Default)]
struct BufferInlays {
    buffer_version: Global,
    inlays_per_excerpts: HashMap<ExcerptId, OrderedByAnchorOffset<(InlayId, InlayHint)>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InlayId(pub usize);

#[derive(Debug, Default)]
pub struct InlaySplice {
    pub to_remove: Vec<InlayId>,
    pub to_insert: Vec<(InlayId, Anchor, InlayHint)>,
}

pub struct QueryInlaysRange {
    pub buffer_id: u64,
    pub buffer_path: PathBuf,
    pub buffer_version: Global,
    pub excerpt_id: ExcerptId,
    pub excerpt_offset_range: Range<usize>,
}

impl InlayCache {
    pub fn new(inlay_hint_settings: editor_settings::InlayHints) -> Self {
        Self {
            inlays_per_buffer: HashMap::default(),
            allowed_hint_kinds: allowed_inlay_hint_types(inlay_hint_settings),
            next_inlay_id: 0,
        }
    }

    pub fn fetch_inlays(
        &mut self,
        multi_buffer: ModelHandle<MultiBuffer>,
        inlay_fetch_ranges: impl Iterator<Item = QueryInlaysRange>,
        cx: &mut ViewContext<Editor>,
    ) -> Task<anyhow::Result<InlaySplice>> {
        let mut inlay_fetch_tasks = Vec::new();
        for inlay_fetch_range in inlay_fetch_ranges {
            let inlays_up_to_date = self.inlays_up_to_date(
                &inlay_fetch_range.buffer_path,
                &inlay_fetch_range.buffer_version,
                inlay_fetch_range.excerpt_id,
            );
            let task_multi_buffer = multi_buffer.clone();
            let task = cx.spawn(|editor, mut cx| async move {
                if inlays_up_to_date {
                    anyhow::Ok((inlay_fetch_range, None))
                } else {
                    let Some(buffer_handle) = cx.read(|cx| task_multi_buffer.read(cx).buffer(inlay_fetch_range.buffer_id))
                        else { return Ok((inlay_fetch_range, Some(Vec::new()))) };
                    let task = editor
                        .update(&mut cx, |editor, cx| {
                            let max_buffer_offset = buffer_handle.read(cx).len();
                            let excerpt_offset_range = &inlay_fetch_range.excerpt_offset_range;
                            editor.project.as_ref().map(|project| {
                                project.update(cx, |project, cx| {
                                    project.query_inlay_hints_for_buffer(
                                        buffer_handle,
                                        excerpt_offset_range.start..excerpt_offset_range.end.min(max_buffer_offset),
                                        cx,
                                    )
                                })
                            })
                        })
                        .context("inlays fecth task spawn")?;

                    Ok((inlay_fetch_range, match task {
                        Some(task) => task.await.context("inlays for buffer task")?,
                        None => Some(Vec::new()),
                    }))
                }
            });
            inlay_fetch_tasks.push(task);
        }

        let final_task = cx.spawn(|editor, mut cx| async move {
            let mut inlay_updates: HashMap<
                PathBuf,
                (
                    Global,
                    HashMap<ExcerptId, Option<(Range<usize>, OrderedByAnchorOffset<InlayHint>)>>,
                ),
            > = HashMap::default();
            let multi_buffer_snapshot =
                editor.read_with(&cx, |editor, cx| editor.buffer().read(cx).snapshot(cx))?;

            for task_result in futures::future::join_all(inlay_fetch_tasks).await {
                match task_result {
                    Ok((request_key, response_inlays)) => {
                        let inlays_per_excerpt = HashMap::from_iter([(
                            request_key.excerpt_id,
                            response_inlays
                                .map(|excerpt_inlays| {
                                    excerpt_inlays.into_iter().fold(
                                        OrderedByAnchorOffset::default(),
                                        |mut ordered_inlays, inlay| {
                                            let anchor = multi_buffer_snapshot.anchor_in_excerpt(
                                                request_key.excerpt_id,
                                                inlay.position,
                                            );
                                            ordered_inlays.add(anchor, inlay);
                                            ordered_inlays
                                        },
                                    )
                                })
                                .map(|inlays| (request_key.excerpt_offset_range, inlays)),
                        )]);
                        match inlay_updates.entry(request_key.buffer_path) {
                            hash_map::Entry::Occupied(mut o) => {
                                o.get_mut().1.extend(inlays_per_excerpt);
                            }
                            hash_map::Entry::Vacant(v) => {
                                v.insert((request_key.buffer_version, inlays_per_excerpt));
                            }
                        }
                    }
                    Err(e) => error!("Failed to update inlays for buffer: {e:#}"),
                }
            }

            let updates = if !inlay_updates.is_empty() {
                let inlays_update = editor.update(&mut cx, |editor, _| {
                    editor.inlay_cache.apply_fetch_inlays(inlay_updates)
                })?;
                inlays_update
            } else {
                InlaySplice::default()
            };

            anyhow::Ok(updates)
        });

        final_task
    }

    fn inlays_up_to_date(
        &self,
        buffer_path: &Path,
        buffer_version: &Global,
        excerpt_id: ExcerptId,
    ) -> bool {
        let Some(buffer_inlays) = self.inlays_per_buffer.get(buffer_path) else { return false };
        let buffer_up_to_date = buffer_version == &buffer_inlays.buffer_version
            || buffer_inlays.buffer_version.changed_since(&buffer_version);
        buffer_up_to_date && buffer_inlays.inlays_per_excerpts.contains_key(&excerpt_id)
    }

    fn apply_fetch_inlays(
        &mut self,
        fetched_inlays: HashMap<
            PathBuf,
            (
                Global,
                HashMap<ExcerptId, Option<(Range<usize>, OrderedByAnchorOffset<InlayHint>)>>,
            ),
        >,
    ) -> InlaySplice {
        let mut old_inlays = self.inlays_per_buffer.clone();
        let mut to_remove = Vec::new();
        let mut to_insert = Vec::new();

        for (buffer_path, (buffer_version, new_buffer_inlays)) in fetched_inlays {
            match old_inlays.remove(&buffer_path) {
                Some(mut old_buffer_inlays) => {
                    for (excerpt_id, new_excerpt_inlays) in new_buffer_inlays {
                        let (_, mut new_excerpt_inlays) = match new_excerpt_inlays {
                            Some((excerpt_offset_range, new_inlays)) => (
                                excerpt_offset_range,
                                new_inlays.into_ordered_elements().fuse().peekable(),
                            ),
                            None => continue,
                        };
                        if self.inlays_up_to_date(&buffer_path, &buffer_version, excerpt_id) {
                            continue;
                        }

                        let self_inlays_per_buffer = self
                            .inlays_per_buffer
                            .get_mut(&buffer_path)
                            .expect("element expected: `old_inlays.remove` returned `Some`");

                        if old_buffer_inlays
                            .inlays_per_excerpts
                            .remove(&excerpt_id)
                            .is_some()
                        {
                            let self_excerpt_inlays = self_inlays_per_buffer
                                .inlays_per_excerpts
                                .get_mut(&excerpt_id)
                                .expect("element expected: `old_excerpt_inlays` is `Some`");
                            let mut hints_to_add = Vec::<(Anchor, (InlayId, InlayHint))>::new();
                            // TODO kb update inner buffer_id and version with the new data?
                            self_excerpt_inlays.0.retain(
                                |_, (old_anchor, (old_inlay_id, old_inlay))| {
                                    let mut retain = false;

                                    while let Some(new_offset) = new_excerpt_inlays
                                        .peek()
                                        .map(|(new_anchor, _)| new_anchor.text_anchor.offset)
                                    {
                                        let old_offset = old_anchor.text_anchor.offset;
                                        match new_offset.cmp(&old_offset) {
                                            cmp::Ordering::Less => {
                                                let (new_anchor, new_inlay) =
                                                    new_excerpt_inlays.next().expect(
                                                        "element expected: `peek` returned `Some`",
                                                    );
                                                hints_to_add.push((
                                                    new_anchor,
                                                    (
                                                        InlayId(post_inc(&mut self.next_inlay_id)),
                                                        new_inlay,
                                                    ),
                                                ));
                                            }
                                            cmp::Ordering::Equal => {
                                                let (new_anchor, new_inlay) =
                                                    new_excerpt_inlays.next().expect(
                                                        "element expected: `peek` returned `Some`",
                                                    );
                                                if &new_inlay == old_inlay {
                                                    retain = true;
                                                } else {
                                                    hints_to_add.push((
                                                        new_anchor,
                                                        (
                                                            InlayId(post_inc(
                                                                &mut self.next_inlay_id,
                                                            )),
                                                            new_inlay,
                                                        ),
                                                    ));
                                                }
                                            }
                                            cmp::Ordering::Greater => break,
                                        }
                                    }

                                    if !retain {
                                        to_remove.push(*old_inlay_id);
                                    }
                                    retain
                                },
                            );

                            for (new_anchor, (id, new_inlay)) in hints_to_add {
                                self_excerpt_inlays.add(new_anchor, (id, new_inlay.clone()));
                                to_insert.push((id, new_anchor, new_inlay));
                            }
                        }

                        for (new_anchor, new_inlay) in new_excerpt_inlays {
                            let id = InlayId(post_inc(&mut self.next_inlay_id));
                            self_inlays_per_buffer
                                .inlays_per_excerpts
                                .entry(excerpt_id)
                                .or_default()
                                .add(new_anchor, (id, new_inlay.clone()));
                            to_insert.push((id, new_anchor, new_inlay));
                        }
                    }
                }
                None => {
                    let mut inlays_per_excerpts: HashMap<
                        ExcerptId,
                        OrderedByAnchorOffset<(InlayId, InlayHint)>,
                    > = HashMap::default();
                    for (new_excerpt_id, new_ordered_inlays) in new_buffer_inlays {
                        if let Some((_, new_ordered_inlays)) = new_ordered_inlays {
                            for (new_anchor, new_inlay) in
                                new_ordered_inlays.into_ordered_elements()
                            {
                                let id = InlayId(post_inc(&mut self.next_inlay_id));
                                inlays_per_excerpts
                                    .entry(new_excerpt_id)
                                    .or_default()
                                    .add(new_anchor, (id, new_inlay.clone()));
                                to_insert.push((id, new_anchor, new_inlay));
                            }
                        }
                    }
                    self.inlays_per_buffer.insert(
                        buffer_path,
                        BufferInlays {
                            buffer_version,
                            inlays_per_excerpts,
                        },
                    );
                }
            }
        }

        for (_, old_buffer_inlays) in old_inlays {
            for (_, old_excerpt_inlays) in old_buffer_inlays.inlays_per_excerpts {
                for (_, (id_to_remove, _)) in old_excerpt_inlays.into_ordered_elements() {
                    to_remove.push(id_to_remove);
                }
            }
        }

        to_insert.retain(|(_, _, new_hint)| self.allowed_hint_kinds.contains(&new_hint.kind));

        InlaySplice {
            to_remove,
            to_insert,
        }
    }

    pub fn apply_settings(
        &mut self,
        inlay_hint_settings: editor_settings::InlayHints,
    ) -> InlaySplice {
        let new_allowed_inlay_hint_types = allowed_inlay_hint_types(inlay_hint_settings);

        let new_allowed_hint_kinds = new_allowed_inlay_hint_types
            .difference(&self.allowed_hint_kinds)
            .copied()
            .collect::<HashSet<_>>();
        let removed_hint_kinds = self
            .allowed_hint_kinds
            .difference(&new_allowed_inlay_hint_types)
            .collect::<HashSet<_>>();
        let mut to_remove = Vec::new();
        let mut to_insert = Vec::new();
        for (anchor, (inlay_id, inlay_hint)) in self
            .inlays_per_buffer
            .iter()
            .map(|(_, buffer_inlays)| {
                buffer_inlays
                    .inlays_per_excerpts
                    .iter()
                    .map(|(_, excerpt_inlays)| excerpt_inlays.ordered_elements())
                    .flatten()
            })
            .flatten()
        {
            if removed_hint_kinds.contains(&inlay_hint.kind) {
                to_remove.push(*inlay_id);
            } else if new_allowed_hint_kinds.contains(&inlay_hint.kind) {
                to_insert.push((*inlay_id, *anchor, inlay_hint.to_owned()));
            }
        }

        self.allowed_hint_kinds = new_allowed_hint_kinds;

        InlaySplice {
            to_remove,
            to_insert,
        }
    }

    pub fn clear(&mut self) -> Vec<InlayId> {
        self.inlays_per_buffer
            .drain()
            .map(|(_, buffer_inlays)| {
                buffer_inlays
                    .inlays_per_excerpts
                    .into_iter()
                    .map(|(_, excerpt_inlays)| {
                        excerpt_inlays
                            .into_ordered_elements()
                            .map(|(_, (id, _))| id)
                    })
                    .flatten()
            })
            .flatten()
            .collect()
    }
}

fn allowed_inlay_hint_types(
    inlay_hint_settings: editor_settings::InlayHints,
) -> HashSet<Option<InlayHintKind>> {
    let mut new_allowed_inlay_hint_types = HashSet::default();
    if inlay_hint_settings.show_type_hints {
        new_allowed_inlay_hint_types.insert(Some(InlayHintKind::Type));
    }
    if inlay_hint_settings.show_parameter_hints {
        new_allowed_inlay_hint_types.insert(Some(InlayHintKind::Parameter));
    }
    if inlay_hint_settings.show_other_hints {
        new_allowed_inlay_hint_types.insert(None);
    }
    new_allowed_inlay_hint_types
}
