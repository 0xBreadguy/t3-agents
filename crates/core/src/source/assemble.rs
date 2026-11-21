//! Snapshot assembly, node/subagent attribution, and per-agent decoration.
//!
//! Keeps both the typed single-pass path (`assemble_typed`, `FastAttributor`)
//! and the shared JSON/cached path (`assemble`, `Attributor`) used by `live.rs`.
use super::{
    Json, SourceError,
    fast_reads::{FetchedTurnItem, HistoryRow, NodeMap, PlanRow, ProjectRow, RequestRow, SpanRow},
    number, obj, optional, problem, str_at, timing, truthy_str,
};
use crate::model::{self, Agent, Context, Event, Item, Point, Shelf, Snapshot, Step};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Instant,
};

struct FastAttributor<'a> {
    sub_ids: HashSet<&'a str>,
    aliases: HashMap<&'a str, &'a str>,
    nodes: &'a NodeMap,
    native: HashMap<&'a str, &'a str>,
}

impl<'a> FastAttributor<'a> {
    fn new(
        agents: &'a [Agent],
        agent_index: &HashMap<&'a str, usize>,
        subs: &'a [Json],
        nodes: &'a NodeMap,
    ) -> Self {
        let mut native = HashMap::new();
        for a in agents {
            if a.kind == "subagent" {
                if let Some(pid) = a.provider_id.as_ref().filter(|p| !p.is_empty()) {
                    native.entry(pid.as_str()).or_insert(a.id.as_str());
                }
            }
        }
        let mut result = Self {
            sub_ids: subs.iter().map(|s| str_at(s, "id")).collect(),
            aliases: subs
                .iter()
                .filter_map(|s| {
                    truthy_str(s, "childThreadId")
                        .map(|_| (str_at(s, "id"), str_at(s, "childThreadId")))
                })
                .collect(),
            nodes,
            native,
        };
        for a in agents {
            for id in a.subagent_ids.iter().flatten() {
                result.aliases.insert(id, &a.id);
            }
        }
        for sub in subs {
            if let (Some(id), Some(pid)) = (
                result.owner(Some(str_at(sub, "id"))).map(str::to_owned),
                sub.get("providerThreadId").and_then(Value::as_str),
            ) {
                let id = agent_index
                    .get(id.as_str())
                    .map(|&i| agents[i].id.as_str())
                    .unwrap_or_else(|| str_at(sub, "id"));
                result.native.entry(pid).or_insert(id);
            }
        }
        result
    }

    fn owner<'b>(&'b self, node: Option<&'b str>) -> Option<&'b str> {
        let first = node.filter(|id| !id.is_empty())?;
        if self.sub_ids.contains(first) {
            return Some(self.aliases.get(first).copied().unwrap_or(first));
        }
        if self.nodes.is_empty() {
            return None;
        }
        let mut cur = self.nodes.get(first).map(String::as_str)?;
        if cur.is_empty() || cur == first {
            return None;
        }
        if self.sub_ids.contains(cur) {
            return Some(self.aliases.get(cur).copied().unwrap_or(cur));
        }
        let mut seen = HashSet::new();
        seen.insert(first);
        seen.insert(cur);
        while let Some(next) = self
            .nodes
            .get(cur)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
        {
            if !seen.insert(next) {
                break;
            }
            if self.sub_ids.contains(next) {
                return Some(self.aliases.get(next).copied().unwrap_or(next));
            }
            cur = next;
        }
        None
    }

    fn route<'b>(
        &'b self,
        provider_thread_id: Option<&'b str>,
        node_id: Option<&'b str>,
        thread_id: &'b str,
    ) -> &'b str {
        provider_thread_id
            .filter(|id| !id.is_empty())
            .and_then(|pid| self.native.get(pid).copied())
            .or_else(|| self.owner(node_id))
            .unwrap_or(thread_id)
    }

    fn route_exact<'b>(
        &'b self,
        provider_thread_id: Option<&'b str>,
        node_id: Option<&'b str>,
        thread_id: &'b str,
    ) -> &'b str {
        provider_thread_id
            .and_then(|pid| self.native.get(pid).copied())
            .or_else(|| self.owner(node_id))
            .unwrap_or(thread_id)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn assemble_typed(
    threads: Vec<Json>,
    latest: HashMap<String, Json>,
    subs: Vec<Json>,
    providers: HashMap<String, Json>,
    turns: HashMap<String, Json>,
    nodes: &NodeMap,
    projects: &HashMap<String, ProjectRow>,
    requests: &[RequestRow],
    items: &[Item],
    events: &[FetchedTurnItem],
    history: &[HistoryRow],
    spans: &[SpanRow],
    plans: &[PlanRow],
    path: &Path,
    now: f64,
) -> Result<Snapshot, SourceError> {
    let _assembly = timing::phase("assembly");
    let build_t = timing::on().then(Instant::now);
    let mut agents = {
        let _structure = timing::phase("structure_build");
        let mut agents = Vec::with_capacity(threads.len());
        for thread in &threads {
            let tid = str_at(thread, "id").to_string();
            let run = latest.get(&tid).cloned().unwrap_or_else(|| json!({}));
            agents.push(thread_agent_typed(
                thread, &run, &providers, &turns, projects, path,
            )?);
        }
        link_structure_typed(
            &mut agents,
            &subs,
            &providers,
            &turns,
            nodes,
            &threads,
            path,
        )?;
        agents
    };

    let content_t = timing::phase("agent_content_build");
    let index: HashMap<String, usize> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| (a.id.clone(), i))
        .collect();
    // Compute all attribution indices ONCE after link_structure_typed: agent
    // kind/provider_id/subagent_ids do not change during body/decorate phases.
    let (item_targets, req_targets, event_targets, history_targets, plan_targets) = {
        let ref_index: HashMap<&str, usize> = agents
            .iter()
            .enumerate()
            .map(|(i, a)| (a.id.as_str(), i))
            .collect();
        let attr = FastAttributor::new(&agents, &ref_index, &subs, nodes);
        let item_targets: Vec<Option<usize>> = items
            .iter()
            .map(|item| {
                let t = attr.route(
                    item.provider_thread_id.as_deref(),
                    item.node_id.as_deref(),
                    &item.thread_id,
                );
                ref_index.get(t).copied()
            })
            .collect();
        let req_targets: Vec<Option<usize>> = requests
            .iter()
            .map(|req| {
                let t = attr.route_exact(None, req.node_id.as_deref(), &req.thread_id);
                ref_index.get(t).copied()
            })
            .collect();
        let event_targets: Vec<Option<usize>> = events
            .iter()
            .map(|row| {
                let t = attr.route_exact(
                    row.provider_thread_id.as_deref(),
                    row.node_id.as_deref(),
                    row.thread_id.as_deref().unwrap_or(""),
                );
                ref_index.get(t).copied()
            })
            .collect();
        let history_targets: Vec<Option<usize>> = history
            .iter()
            .map(|row| {
                let t =
                    attr.route_exact(Some(row.provider_thread_id.as_str()), None, &row.thread_id);
                ref_index.get(t).copied()
            })
            .collect();
        let plan_targets: Vec<Option<usize>> = plans
            .iter()
            .map(|row| {
                let t = attr.route_exact(
                    row.provider_thread_id.as_deref(),
                    row.node_id.as_deref(),
                    &row.thread_id,
                );
                ref_index.get(t).copied()
            })
            .collect();
        (
            item_targets,
            req_targets,
            event_targets,
            history_targets,
            plan_targets,
        )
    };

    for (item, target) in items.iter().zip(item_targets) {
        if let Some(i) = target {
            attach_item(&mut agents[i], item);
        }
    }
    for (req, target) in requests.iter().zip(req_targets) {
        if let Some(i) = target {
            attach_request(
                &mut agents[i],
                &req.kind,
                req.created_at.as_deref(),
                req.async_input,
            );
        }
    }
    let known: HashSet<String> = agents.iter().map(|a| a.id.clone()).collect();
    for a in agents.iter_mut() {
        finish_agent_body(a, &known);
    }
    drop(content_t);
    {
        let _cycles = timing::phase("structure_cycles");
        break_cycles(&mut agents);
    }
    if let Some(t) = build_t {
        timing::add_build(t);
    }

    let dec_t = timing::on().then(Instant::now);
    {
        let _dec_phase = timing::phase("agent_content_decorate");
        for (row, target) in events.iter().zip(event_targets) {
            let failed = row.status.as_deref() == Some("failed");
            let updated = row.updated_at.as_deref();
            let started = row.started_at.as_deref();
            let kind = row.kind.as_deref().unwrap_or("");
            let tool = row.tool.as_deref();
            let time =
                model::ms(if failed { updated } else { started }).or_else(|| model::ms(updated));
            if let (Some(i), Some(t)) = (target, time) {
                let failed_item = if failed {
                    Some(row.to_item(path)?)
                } else {
                    None
                };
                attach_event(&mut agents[i], t, kind, tool, failed, failed_item.as_ref());
            }
        }
        for (row, target) in history.iter().zip(history_targets) {
            if let Some(i) = target {
                attach_history(
                    &mut agents[i],
                    row.used,
                    row.max,
                    model::ms(row.started_at.as_deref())
                        .or_else(|| model::ms(row.completed_at.as_deref()))
                        .unwrap_or(0.),
                    &row.provider_thread_id,
                    row.model.as_deref().unwrap_or(""),
                );
            }
        }
        for row in spans {
            if let Some(&i) = index.get(&row.thread_id) {
                attach_span(
                    &mut agents[i],
                    row.started_at.as_deref(),
                    row.requested_at.as_deref(),
                    row.completed_at.as_deref(),
                );
            }
        }
        for a in agents.iter_mut() {
            finish_agent_tail(a);
        }
        for (row, target) in plans.iter().zip(plan_targets) {
            if let Some(i) = target {
                consider_plan(&mut agents[i], row.run_id.as_deref(), row.steps.as_deref());
            }
        }
    }
    if let Some(t) = dec_t {
        timing::add_decorate(t);
    }

    Ok(Snapshot {
        agents: agents.into_iter().map(std::sync::Arc::new).collect(),
        at: now,
        path: path.to_string_lossy().into(),
    })
}

fn thread_agent_typed(
    thread: &Json,
    run: &Json,
    providers: &HashMap<String, Json>,
    turns: &HashMap<String, Json>,
    projects: &HashMap<String, ProjectRow>,
    path: &Path,
) -> Result<Agent, SourceError> {
    let tid = str_at(thread, "id").to_string();
    let pid = truthy_str(run, "providerThreadId")
        .or_else(|| truthy_str(thread, "activeProviderThreadId"));
    let (ctx, turn) = context(pid.as_deref(), providers, turns, path)?;
    let proj_id = str_at(thread, "projectId");
    let project = projects.get(proj_id);
    let storage = model::ThreadStorage {
        project_id: proj_id.into(),
        project_root: project.and_then(|p| p.workspace_root.clone()),
        worktree: truthy_str(thread, "worktreePath"),
    };
    let lineage = obj(thread, "lineage");
    let selection = if run.get("modelSelection").is_some_and(|v| !v.is_null()) {
        obj(run, "modelSelection")
    } else {
        obj(thread, "modelSelection")
    };
    Ok(Agent {
        id: tid.clone(),
        parent: if str_at(&lineage, "relationshipToParent") == "subagent" {
            optional(&lineage, "parentThreadId")
        } else {
            None
        },
        title: optional(thread, "title").unwrap_or_else(|| "Untitled".into()),
        kind: "thread".into(),
        project: project
            .and_then(|p| p.title.clone())
            .unwrap_or_else(|| "Unknown project".into()),
        cwd: storage
            .worktree
            .as_ref()
            .or(storage.project_root.as_ref())
            .cloned()
            .unwrap_or_default(),
        storage: Some(storage),
        model: truthy_str(&turn, "model")
            .or_else(|| truthy_str(&selection, "model"))
            .unwrap_or_else(|| "--".into()),
        status: optional(run, "status").unwrap_or_else(|| "idle".into()),
        run: run.clone(),
        provider_id: pid,
        context: ctx,
        turn,
        updated: optional(thread, "updatedAt"),
        started: truthy_str(run, "startedAt").or_else(|| truthy_str(run, "requestedAt")),
        ended: optional(run, "completedAt"),
        remote: None,
        thread_id: tid,
        branch: optional(thread, "branch"),
        options: selection
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        shelf: Some(Shelf {
            listed: str_at(&lineage, "relationshipToParent") != "subagent"
                && lineage.get("sideChat") != Some(&Value::Bool(true)),
            settled: str_at(thread, "settledOverride") == "settled",
            snoozed_until: optional(thread, "snoozedUntil"),
        }),
        ..Agent::default()
    })
}

fn link_structure_typed(
    agents: &mut Vec<Agent>,
    subs: &[Json],
    providers: &HashMap<String, Json>,
    turns: &HashMap<String, Json>,
    nodes: &NodeMap,
    threads: &[Json],
    path: &Path,
) -> Result<(), SourceError> {
    let mut index: HashMap<String, usize> = agents
        .iter()
        .enumerate()
        .map(|(i, agent)| (agent.id.clone(), i))
        .collect();
    let local_ids: HashSet<String> = index.keys().cloned().collect();
    let local_env = crate::integration::environment_id(path);
    let key = |v: &Value| {
        crate::integration::thread_ref(v).map(|(e, t)| serde_json::to_string(&(e, t)).unwrap())
    };
    let mut remote_ids: HashMap<String, String> = HashMap::new();
    let mut ordered_subs: Vec<&Json> = subs.iter().collect();
    ordered_subs.sort_by(|a, b| {
        let stamp = |s: &Json| {
            optional(s, "updatedAt")
                .or_else(|| optional(s, "startedAt"))
                .unwrap_or_default()
        };
        crate::terminal::locale_compare(&stamp(b), &stamp(a))
            .then_with(|| crate::terminal::locale_compare(str_at(a, "id"), str_at(b, "id")))
    });
    let mut preferred = HashMap::new();
    for sub in &ordered_subs {
        if let Some(k) = sub.get("remoteChild").and_then(&key) {
            preferred.entry(k).or_insert(*sub);
        }
    }
    for entry in subs {
        let ref_key = entry.get("remoteChild").and_then(&key);
        let sub = ref_key
            .as_ref()
            .and_then(|k| preferred.get(k))
            .copied()
            .unwrap_or(entry);
        let known = entry
            .get("remoteChild")
            .and_then(crate::integration::thread_ref)
            .and_then(|(e, t)| {
                if Some(e) == local_env.as_deref() && local_ids.contains(t) {
                    Some(t.to_owned())
                } else {
                    ref_key.as_ref().and_then(|k| remote_ids.get(k).cloned())
                }
            });
        let existing = index
            .get(str_at(entry, "childThreadId"))
            .copied()
            .or_else(|| known.as_ref().and_then(|k| index.get(k).copied()));
        if let Some(i) = existing {
            agents[i]
                .subagent_ids
                .get_or_insert_with(Vec::new)
                .push(str_at(entry, "id").into());
            if let Some(k) = ref_key {
                remote_ids.insert(k, agents[i].id.clone());
            }
            continue;
        }
        let pid = optional(sub, "providerThreadId");
        let (ctx, turn) = context(pid.as_deref(), providers, turns, path)?;
        let id = str_at(sub, "id").to_string();
        let project = index
            .get(str_at(sub, "threadId"))
            .map(|i| agents[*i].project.clone())
            .unwrap_or_default();
        let a = Agent {
            id: id.clone(),
            parent: None,
            subagent_ids: Some(vec![str_at(entry, "id").into()]),
            title: truthy_str(sub, "title").unwrap_or_else(|| "Subagent".into()),
            kind: "subagent".into(),
            project,
            cwd: String::new(),
            model: truthy_str(&turn, "model")
                .or_else(|| truthy_str(sub, "model"))
                .unwrap_or_else(|| "--".into()),
            status: optional(sub, "status").unwrap_or_else(|| "unknown".into()),
            run: json!({"id":sub.get("runId").cloned().unwrap_or(Value::Null)}),
            provider_id: pid,
            context: ctx,
            turn,
            updated: optional(sub, "updatedAt"),
            started: optional(sub, "startedAt"),
            ended: optional(sub, "completedAt"),
            remote: sub.get("remoteChild").filter(|v| !v.is_null()).cloned(),
            thread_id: str_at(sub, "threadId").into(),
            ..Agent::default()
        };
        if let Some(k) = ref_key {
            remote_ids.insert(k, id.clone());
        }
        if let Some(i) = index.get(&id).copied() {
            agents[i] = a;
        } else {
            index.insert(id, agents.len());
            agents.push(a);
        }
    }
    let parents = {
        let ref_index: HashMap<&str, usize> = agents
            .iter()
            .enumerate()
            .map(|(i, a)| (a.id.as_str(), i))
            .collect();
        let attr = FastAttributor::new(agents, &ref_index, subs, nodes);
        let mut parented = HashSet::new();
        ordered_subs
            .iter()
            .filter_map(|sub| {
                let id = attr.owner(Some(str_at(sub, "id")))?.to_owned();
                if !parented.insert(id.clone()) {
                    return None;
                }
                let parent = attr
                    .owner(sub.get("parentNodeId").and_then(Value::as_str))
                    .map(str::to_owned)
                    .or_else(|| optional(sub, "threadId"));
                Some((id, parent))
            })
            .collect::<Vec<_>>()
    };
    for (id, parent) in parents {
        if let Some(i) = index.get(&id) {
            agents[*i].parent = parent;
            agents[*i].kind = "subagent".into();
        }
    }
    for thread in threads {
        let Some((env, tid)) = thread
            .get("delegatedFrom")
            .and_then(crate::integration::thread_ref)
        else {
            continue;
        };
        let i = index[str_at(thread, "id")];
        let lineage = obj(thread, "lineage");
        if str_at(&lineage, "relationshipToParent") == "subagent"
            && local_ids.contains(str_at(&lineage, "parentThreadId"))
        {
            continue;
        }
        let k = serde_json::to_string(&(env, tid)).unwrap();
        let parent = if Some(env) == local_env.as_deref() && local_ids.contains(tid) {
            tid.to_owned()
        } else if let Some(id) = remote_ids.get(&k) {
            id.clone()
        } else {
            let mut id = format!("remote:{k}");
            while index.contains_key(&id) {
                id.push_str(":ref");
            }
            let a = Agent {
                id: id.clone(),
                title: truthy_str(&obj(thread, "delegatedFrom"), "title")
                    .unwrap_or("Remote parent".into()),
                kind: "thread".into(),
                project: agents[i].project.clone(),
                model: "--".into(),
                status: "unknown".into(),
                run: json!({}),
                turn: json!({}),
                remote: Some(json!({"environmentId":env,"threadId":tid})),
                reference_only: Some(true),
                thread_id: tid.into(),
                ..Agent::default()
            };
            index.insert(id.clone(), agents.len());
            agents.push(a);
            remote_ids.insert(k, id.clone());
            id
        };
        agents[i].parent = Some(parent);
        agents[i].kind = "subagent".into();
    }
    Ok(())
}

/// SQL rows remain owned JSON; cached items are borrowed in place. The oracle
/// still decodes only selected items and failed events, as before.
pub(crate) enum ItemRows<'a> {
    Json(Vec<Json>),
    Typed(Vec<&'a Item>),
}
impl From<Vec<Json>> for ItemRows<'_> {
    fn from(rows: Vec<Json>) -> Self {
        Self::Json(rows)
    }
}
impl<'a> From<Vec<&'a Item>> for ItemRows<'a> {
    fn from(rows: Vec<&'a Item>) -> Self {
        Self::Typed(rows)
    }
}

/// Shared assembly boundary: both SQL-selected and cache-selected narrow rows.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble<'a>(
    threads: Vec<Json>,
    latest: HashMap<String, Json>,
    subs: Vec<Json>,
    providers: HashMap<String, Json>,
    turns: HashMap<String, Json>,
    nodes: HashMap<String, Json>,
    projects: HashMap<String, Json>,
    requests: Vec<Json>,
    items: impl Into<ItemRows<'a>>,
    events: impl Into<ItemRows<'a>>,
    history: Vec<Json>,
    spans: Vec<Json>,
    plans: Vec<Json>,
    path: &Path,
    now: f64,
) -> Result<Snapshot, SourceError> {
    let _assembly = timing::phase("assembly");
    let owned;
    let items = match items.into() {
        ItemRows::Json(rows) => {
            let timed = timing::on().then(Instant::now);
            owned = rows
                .into_iter()
                .map(|row| {
                    serde_json::from_value(row).map_err(|e| problem(path, "schema", e.to_string()))
                })
                .collect::<Result<Vec<Item>, _>>()?;
            if let Some(t) = timed {
                timing::add_json(t);
            }
            owned.iter().collect::<Vec<_>>()
        }
        ItemRows::Typed(rows) => rows,
    };
    let events = events.into();
    let timed = timing::on().then(Instant::now);
    let mut agents = build_agents(
        &threads, &latest, &subs, &providers, &turns, &nodes, &projects, &requests, &items, path,
    )?;
    if let Some(t) = timed {
        timing::add_build(t);
    }
    let timed = timing::on().then(Instant::now);
    let decorated = decorate(
        &mut agents,
        &subs,
        &nodes,
        &events,
        &history,
        &spans,
        &plans,
        path,
    );
    if let Some(t) = timed {
        timing::add_decorate(t);
    }
    decorated?;
    Ok(Snapshot {
        agents: agents.into_iter().map(std::sync::Arc::new).collect(),
        at: now,
        path: path.to_string_lossy().into(),
    })
}

pub(crate) struct Attributor<'a> {
    sub_ids: HashSet<&'a str>,
    aliases: HashMap<&'a str, &'a str>,
    nodes: &'a HashMap<String, Json>,
    native: HashMap<&'a str, &'a str>,
}
impl<'a> Attributor<'a> {
    pub(crate) fn new(
        agents: &'a [Agent],
        subs: &'a [Json],
        nodes: &'a HashMap<String, Json>,
    ) -> Self {
        let mut native = HashMap::new();
        for a in agents {
            if a.kind == "subagent" {
                if let Some(pid) = a.provider_id.as_ref().filter(|p| !p.is_empty()) {
                    native.entry(pid.as_str()).or_insert(a.id.as_str());
                }
            }
        }
        let mut result = Self {
            sub_ids: subs.iter().map(|s| str_at(s, "id")).collect(),
            aliases: subs
                .iter()
                .filter_map(|s| {
                    truthy_str(s, "childThreadId")
                        .map(|_| (str_at(s, "id"), str_at(s, "childThreadId")))
                })
                .collect(),
            nodes,
            native,
        };
        for a in agents {
            for id in a.subagent_ids.iter().flatten() {
                result.aliases.insert(id, &a.id);
            }
        }
        for sub in subs {
            if let (Some(id), Some(pid)) = (
                result.owner(Some(str_at(sub, "id"))).map(str::to_owned),
                sub.get("providerThreadId").and_then(Value::as_str),
            ) {
                // Resolve back to an input-backed reference for the map's lifetime.
                let id = agents
                    .iter()
                    .find(|a| a.id == id)
                    .map(|a| a.id.as_str())
                    .unwrap_or_else(|| str_at(sub, "id"));
                result.native.entry(pid).or_insert(id);
            }
        }
        result
    }
    pub(crate) fn owner<'b>(&'b self, node: Option<&'b str>) -> Option<&'b str> {
        let mut node = node;
        let mut seen = HashSet::new();
        while let Some(id) = node.filter(|id| !id.is_empty()) {
            if !seen.insert(id) {
                break;
            }
            if self.sub_ids.contains(id) {
                return Some(self.aliases.get(id).copied().unwrap_or(id));
            }
            node = self
                .nodes
                .get(id)
                .and_then(|r| r.get("parent_node_id"))
                .and_then(Value::as_str);
        }
        None
    }
    pub(crate) fn item_target<'b>(&'b self, row: &'b Item) -> &'b str {
        self.route_ids(
            row.provider_thread_id.as_deref(),
            row.node_id.as_deref(),
            &row.thread_id,
        )
    }
    /// Same attribution as `item_target`, from the three id fields alone.
    pub(crate) fn route_ids<'b>(
        &'b self,
        provider_thread_id: Option<&'b str>,
        node_id: Option<&'b str>,
        thread_id: &'b str,
    ) -> &'b str {
        provider_thread_id
            .filter(|id| !id.is_empty())
            .and_then(|pid| self.native.get(pid).copied())
            .or_else(|| self.owner(node_id))
            .unwrap_or(thread_id)
    }
    pub(crate) fn target<'b>(&'b self, row: &'b Json) -> &'b str {
        row.get("provider_thread_id")
            .and_then(Value::as_str)
            .and_then(|pid| self.native.get(pid).copied())
            .or_else(|| self.owner(row.get("node_id").and_then(Value::as_str)))
            .unwrap_or_else(|| str_at(row, "thread_id"))
    }
}

pub(crate) fn context(
    pid: Option<&str>,
    providers: &HashMap<String, Json>,
    turns: &HashMap<String, Json>,
    path: &Path,
) -> Result<(Context, Json), SourceError> {
    let pid = pid.filter(|id| !id.is_empty());
    let p = pid.and_then(|id| providers.get(id));
    let turn = pid
        .and_then(|id| turns.get(id))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let fallback = p
        .map(|p| obj(p, "contextUsage"))
        .unwrap_or_else(|| json!({}));
    let mut ctx = turn
        .get("tokenUsage")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| fallback.clone());
    if ctx.get("usedTokens").is_none_or(Value::is_null) {
        ctx = fallback;
    }
    Ok((
        serde_json::from_value(ctx).map_err(|e| problem(path, "schema", e.to_string()))?,
        turn,
    ))
}

/// One definition of the heavy per-agent fields. The SQL oracle and the live
/// cache both call these, so a capped item, event, history point, span or plan
/// cannot be assembled two different ways.
pub(crate) fn attach_item(agent: &mut Agent, item: &Item) {
    if item.run_id.as_deref() == agent.run.get("id").and_then(Value::as_str) {
        agent.items.push(item.clone());
    }
}
pub(crate) fn attach_request(
    agent: &mut Agent,
    kind: &str,
    created_at: Option<&str>,
    async_input: bool,
) {
    if kind == "user_input" {
        if async_input {
            agent.open_asks.nonblocking += 1;
        } else {
            agent.open_asks.blocking += 1;
        }
    }
    if !async_input {
        agent.requests.push(kind.to_string());
    }
    if let Some(asked) = created_at.filter(|s| !s.is_empty()) {
        if agent.asked.as_ref().is_none_or(|old| asked < old.as_str()) {
            agent.asked = Some(asked.to_string());
        }
    }
}
pub(crate) fn finish_agent_body(agent: &mut Agent, known: &HashSet<String>) {
    agent.items.sort_by(|p, q| q.updated_at.cmp(&p.updated_at));
    if let Some(item) = agent.items.first() {
        if item.updated_at.as_str() > agent.updated.as_deref().unwrap_or("") {
            agent.updated = Some(item.updated_at.clone());
        }
    }
    if !agent.requests.is_empty() && model::live(agent) {
        agent.status = "waiting".into();
    }
    if agent
        .parent
        .as_ref()
        .is_none_or(|parent| parent == &agent.id || !known.contains(parent))
    {
        agent.parent = None;
    }
    let current = || {
        agent.items.iter().filter(|i| {
            live_status(&i.status)
                && !matches!(i.kind.as_str(), "secret_request" | "link_request")
                && !(agent.open_asks.nonblocking > 0 && i.kind == "user_input_request")
        })
    };
    agent.now = current()
        .find(|i| !matches!(i.kind.as_str(), "reasoning" | "assistant_message"))
        .or_else(|| current().next())
        .cloned();
    for item in &agent.items {
        if matches!(item.kind.as_str(), "reasoning" | "assistant_message") {
            continue;
        }
        *agent
            .mix
            .entry(model::category(&item.kind, item.tool.as_deref()).into())
            .or_insert(0) += 1;
        if item.status == "failed" {
            agent.failures += 1;
        }
    }
}
pub(crate) fn attach_event(
    agent: &mut Agent,
    at: f64,
    kind: &str,
    tool: Option<&str>,
    failed: bool,
    failed_item: Option<&Item>,
) {
    agent.events.push(Event {
        t: at,
        cat: model::category(kind, tool).into(),
        failed,
    });
    if failed {
        if let Some(item) = failed_item {
            agent.recent_failures.push(item.clone());
        }
    }
}
pub(crate) fn attach_history(
    agent: &mut Agent,
    used: f64,
    max: f64,
    at: f64,
    provider: &str,
    model_name: &str,
) {
    if used.is_finite() && max.is_finite() && max > 0. {
        agent.history.push(Point {
            pct: (used / max).min(1.),
            used,
            max,
            at,
            provider: provider.into(),
            model: model_name.into(),
        });
    }
}
pub(crate) fn consider_plan(agent: &mut Agent, run_id: Option<&str>, steps_json: Option<&str>) {
    if agent.plan.is_some() || run_id != agent.run.get("id").and_then(Value::as_str) {
        return;
    }
    if let Ok(Value::Array(steps)) = serde_json::from_str::<Value>(steps_json.unwrap_or("[]")) {
        if !steps.is_empty() {
            agent.plan = Some(
                steps
                    .iter()
                    .map(|s| Step {
                        text: optional(s, "text")
                            .or_else(|| optional(s, "step"))
                            .unwrap_or_default(),
                        status: optional(s, "status").unwrap_or_else(|| "pending".into()),
                    })
                    .collect(),
            );
        }
    }
}
pub(crate) fn attach_span(
    agent: &mut Agent,
    started_at: Option<&str>,
    requested_at: Option<&str>,
    completed_at: Option<&str>,
) {
    let start = model::ms(started_at).or_else(|| model::ms(requested_at));
    if let Some(start) = start {
        agent.spans.push((start, model::ms(completed_at)));
    }
}
pub(crate) fn finish_agent_tail(agent: &mut Agent) {
    agent.history.sort_by(|p, q| p.at.total_cmp(&q.at));
    agent.events.sort_by(|p, q| p.t.total_cmp(&q.t));
    if agent.id != agent.thread_id {
        if let Some(start) = model::ms(agent.started.as_deref()) {
            agent.spans.push((start, model::ms(agent.ended.as_deref())));
        }
    }
}

/// Dangling-parent clear and cycle break, the structural half of `finish_agent_body`
/// plus `break_cycles`. Content does not feed either step, so the live reader can
/// hash these parents before it rebuilds any agent's items.
pub(crate) fn seal_parents(agents: &mut [Agent]) {
    let known: HashSet<String> = agents.iter().map(|a| a.id.clone()).collect();
    for agent in agents.iter_mut() {
        if agent
            .parent
            .as_ref()
            .is_none_or(|parent| parent == &agent.id || !known.contains(parent))
        {
            agent.parent = None;
        }
    }
    break_cycles(agents);
}
pub(crate) fn break_cycles(agents: &mut [Agent]) {
    let index: HashMap<String, usize> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| (a.id.clone(), i))
        .collect();
    let mut done = HashSet::new();
    for start in 0..agents.len() {
        let mut path = Vec::new();
        let mut current = Some(start);
        while let Some(i) = current {
            if done.contains(&i) {
                break;
            }
            if let Some(at) = path.iter().position(|p| *p == i) {
                let cut = *path[at..]
                    .iter()
                    .min_by_key(|i: &&usize| &agents[**i].id)
                    .unwrap();
                agents[cut].parent = None;
                break;
            }
            path.push(i);
            current = agents[i]
                .parent
                .as_ref()
                .and_then(|id| index.get(id).copied());
        }
        done.extend(path);
    }
}

fn build_agents(
    threads: &[Json],
    latest: &HashMap<String, Json>,
    subs: &[Json],
    providers: &HashMap<String, Json>,
    turns: &HashMap<String, Json>,
    nodes: &HashMap<String, Json>,
    projects: &HashMap<String, Json>,
    requests: &[Json],
    items: &[&Item],
    path: &Path,
) -> Result<Vec<Agent>, SourceError> {
    let mut agents = build_structure(
        threads, latest, subs, providers, turns, nodes, projects, path,
    )?;
    apply_attributed_body(&mut agents, subs, nodes, requests, items);
    Ok(agents)
}

/// One thread's shell, before subagent links, remote placeholders and cycle breaking.
/// `build_structure` and the live reader both use this, so a patched shell cannot
/// grow a different run, context or shelf than the oracle's full walk.
pub(crate) fn thread_agent(
    thread: &Json,
    run: &Json,
    providers: &HashMap<String, Json>,
    turns: &HashMap<String, Json>,
    projects: &HashMap<String, Json>,
    path: &Path,
) -> Result<Agent, SourceError> {
    let tid = str_at(thread, "id").to_string();
    let pid = truthy_str(run, "providerThreadId")
        .or_else(|| truthy_str(thread, "activeProviderThreadId"));
    let (ctx, turn) = context(pid.as_deref(), providers, turns, path)?;
    let project = projects
        .get(str_at(thread, "projectId"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let storage = model::ThreadStorage {
        project_id: str_at(thread, "projectId").into(),
        project_root: truthy_str(&project, "workspace_root"),
        worktree: truthy_str(thread, "worktreePath"),
    };
    let lineage = obj(thread, "lineage");
    let selection = if run.get("modelSelection").is_some_and(|v| !v.is_null()) {
        obj(run, "modelSelection")
    } else {
        obj(thread, "modelSelection")
    };
    Ok(Agent {
        id: tid.clone(),
        parent: if str_at(&lineage, "relationshipToParent") == "subagent" {
            optional(&lineage, "parentThreadId")
        } else {
            None
        },
        title: optional(thread, "title").unwrap_or_else(|| "Untitled".into()),
        kind: "thread".into(),
        project: optional(&project, "title").unwrap_or_else(|| "Unknown project".into()),
        cwd: storage
            .worktree
            .as_ref()
            .or(storage.project_root.as_ref())
            .cloned()
            .unwrap_or_default(),
        storage: Some(storage),
        model: truthy_str(&turn, "model")
            .or_else(|| truthy_str(&selection, "model"))
            .unwrap_or_else(|| "--".into()),
        status: optional(run, "status").unwrap_or_else(|| "idle".into()),
        run: run.clone(),
        provider_id: pid,
        context: ctx,
        turn,
        updated: optional(thread, "updatedAt"),
        started: truthy_str(run, "startedAt").or_else(|| truthy_str(run, "requestedAt")),
        ended: optional(run, "completedAt"),
        remote: None,
        thread_id: tid,
        branch: optional(thread, "branch"),
        options: selection
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        shelf: Some(Shelf {
            listed: str_at(&lineage, "relationshipToParent") != "subagent"
                && lineage.get("sideChat") != Some(&Value::Bool(true)),
            settled: str_at(thread, "settledOverride") == "settled",
            snoozed_until: optional(thread, "snoozedUntil"),
        }),
        ..Agent::default()
    })
}

/// Identity, parent, kind and remote placeholders. No items, events, requests or plans.
pub(crate) fn build_structure(
    threads: &[Json],
    latest: &HashMap<String, Json>,
    subs: &[Json],
    providers: &HashMap<String, Json>,
    turns: &HashMap<String, Json>,
    nodes: &HashMap<String, Json>,
    projects: &HashMap<String, Json>,
    path: &Path,
) -> Result<Vec<Agent>, SourceError> {
    let structure = timing::phase("structure_build");
    let mut agents = Vec::new();
    for thread in threads {
        let tid = str_at(thread, "id").to_string();
        let run = latest.get(&tid).cloned().unwrap_or_else(|| json!({}));
        let agent = thread_agent(thread, &run, providers, turns, projects, path)?;
        agents.push(agent);
    }
    link_structure(&mut agents, subs, providers, turns, nodes, threads, path)?;
    drop(structure);
    Ok(agents)
}

/// Parents, subagent agents and remote placeholders. `agents` must already be the
/// active thread shells in the oracle's thread order, with link fields still at
/// `thread_agent` (kind `thread`, no subagent ids). `build_structure` calls this
/// for the full walk. A graph poll that can keep its shells does not.
pub(crate) fn link_structure(
    agents: &mut Vec<Agent>,
    subs: &[Json],
    providers: &HashMap<String, Json>,
    turns: &HashMap<String, Json>,
    nodes: &HashMap<String, Json>,
    threads: &[Json],
    path: &Path,
) -> Result<(), SourceError> {
    let mut index: HashMap<String, usize> = agents
        .iter()
        .enumerate()
        .map(|(i, agent)| (agent.id.clone(), i))
        .collect();
    let local_ids: HashSet<String> = index.keys().cloned().collect();
    let local_env = crate::integration::environment_id(path);
    let key = |v: &Value| {
        crate::integration::thread_ref(v).map(|(e, t)| serde_json::to_string(&(e, t)).unwrap())
    };
    let mut remote_ids: HashMap<String, String> = HashMap::new();
    let mut ordered_subs: Vec<&Json> = subs.iter().collect();
    ordered_subs.sort_by(|a, b| {
        let stamp = |s: &Json| {
            optional(s, "updatedAt")
                .or_else(|| optional(s, "startedAt"))
                .unwrap_or_default()
        };
        // `localeCompare` in the original: ICU order, not bytes, so mixed case and
        // punctuation sort as Node sorts them.
        crate::terminal::locale_compare(&stamp(b), &stamp(a))
            .then_with(|| crate::terminal::locale_compare(str_at(a, "id"), str_at(b, "id")))
    });
    let mut preferred = HashMap::new();
    for sub in &ordered_subs {
        if let Some(k) = sub.get("remoteChild").and_then(&key) {
            preferred.entry(k).or_insert(*sub);
        }
    }
    for entry in subs {
        let ref_key = entry.get("remoteChild").and_then(&key);
        let sub = ref_key
            .as_ref()
            .and_then(|k| preferred.get(k))
            .copied()
            .unwrap_or(entry);
        let known = entry
            .get("remoteChild")
            .and_then(crate::integration::thread_ref)
            .and_then(|(e, t)| {
                if Some(e) == local_env.as_deref() && local_ids.contains(t) {
                    Some(t.to_owned())
                } else {
                    ref_key.as_ref().and_then(|k| remote_ids.get(k).cloned())
                }
            });
        let existing = index
            .get(str_at(entry, "childThreadId"))
            .copied()
            .or_else(|| known.as_ref().and_then(|k| index.get(k).copied()));
        if let Some(i) = existing {
            agents[i]
                .subagent_ids
                .get_or_insert_with(Vec::new)
                .push(str_at(entry, "id").into());
            if let Some(k) = ref_key {
                remote_ids.insert(k, agents[i].id.clone());
            }
            continue;
        }
        let pid = optional(sub, "providerThreadId");
        let (ctx, turn) = context(pid.as_deref(), providers, turns, path)?;
        let id = str_at(sub, "id").to_string();
        let project = index
            .get(str_at(sub, "threadId"))
            .map(|i| agents[*i].project.clone())
            .unwrap_or_default();
        let a = Agent {
            id: id.clone(),
            parent: None,
            subagent_ids: Some(vec![str_at(entry, "id").into()]),
            title: truthy_str(sub, "title").unwrap_or_else(|| "Subagent".into()),
            kind: "subagent".into(),
            project,
            cwd: String::new(),
            model: truthy_str(&turn, "model")
                .or_else(|| truthy_str(sub, "model"))
                .unwrap_or_else(|| "--".into()),
            status: optional(sub, "status").unwrap_or_else(|| "unknown".into()),
            run: json!({"id":sub.get("runId").cloned().unwrap_or(Value::Null)}),
            provider_id: pid,
            context: ctx,
            turn,
            updated: optional(sub, "updatedAt"),
            started: optional(sub, "startedAt"),
            ended: optional(sub, "completedAt"),
            remote: sub.get("remoteChild").filter(|v| !v.is_null()).cloned(),
            thread_id: str_at(sub, "threadId").into(),
            ..Agent::default()
        };
        if let Some(k) = ref_key {
            remote_ids.insert(k, id.clone());
        }
        if let Some(i) = index.get(&id).copied() {
            agents[i] = a;
        } else {
            index.insert(id, agents.len());
            agents.push(a);
        }
    }
    let parents = {
        let attr = Attributor::new(agents, subs, nodes);
        let mut parented = HashSet::new();
        ordered_subs
            .iter()
            .filter_map(|sub| {
                let id = attr.owner(Some(str_at(sub, "id")))?.to_owned();
                if !parented.insert(id.clone()) {
                    return None;
                }
                let parent = attr
                    .owner(sub.get("parentNodeId").and_then(Value::as_str))
                    .map(str::to_owned)
                    .or_else(|| optional(sub, "threadId"));
                Some((id, parent))
            })
            .collect::<Vec<_>>()
    };
    for (id, parent) in parents {
        if let Some(i) = index.get(&id) {
            agents[*i].parent = parent;
            agents[*i].kind = "subagent".into();
        }
    }
    for thread in threads {
        let Some((env, tid)) = thread
            .get("delegatedFrom")
            .and_then(crate::integration::thread_ref)
        else {
            continue;
        };
        let i = index[str_at(thread, "id")];
        let lineage = obj(thread, "lineage");
        if str_at(&lineage, "relationshipToParent") == "subagent"
            && local_ids.contains(str_at(&lineage, "parentThreadId"))
        {
            continue;
        }
        let k = serde_json::to_string(&(env, tid)).unwrap();
        let parent = if Some(env) == local_env.as_deref() && local_ids.contains(tid) {
            tid.to_owned()
        } else if let Some(id) = remote_ids.get(&k) {
            id.clone()
        } else {
            let mut id = format!("remote:{k}");
            while index.contains_key(&id) {
                id.push_str(":ref");
            }
            let a = Agent {
                id: id.clone(),
                title: truthy_str(&obj(thread, "delegatedFrom"), "title")
                    .unwrap_or("Remote parent".into()),
                kind: "thread".into(),
                project: agents[i].project.clone(),
                model: "--".into(),
                status: "unknown".into(),
                run: json!({}),
                turn: json!({}),
                remote: Some(json!({"environmentId":env,"threadId":tid})),
                reference_only: Some(true),
                thread_id: tid.into(),
                ..Agent::default()
            };
            index.insert(id.clone(), agents.len());
            agents.push(a);
            remote_ids.insert(k, id.clone());
            id
        };
        agents[i].parent = Some(parent);
        agents[i].kind = "subagent".into();
    }
    Ok(())
}

pub(crate) fn apply_attributed_body(
    agents: &mut [Agent],
    subs: &[Json],
    nodes: &HashMap<String, Json>,
    requests: &[Json],
    items: &[&Item],
) {
    let content = timing::phase("agent_content_build");
    let index: HashMap<String, usize> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| (a.id.clone(), i))
        .collect();
    let targets = {
        let attr = Attributor::new(agents, subs, nodes);
        items
            .iter()
            .map(|item| attr.item_target(item).to_owned())
            .collect::<Vec<_>>()
    };
    for (item, target) in items.iter().zip(targets) {
        if let Some(i) = index.get(&target).copied() {
            attach_item(&mut agents[i], item);
        }
    }
    let targets = {
        let attr = Attributor::new(agents, subs, nodes);
        requests
            .iter()
            .map(|row| attr.target(row).to_owned())
            .collect::<Vec<_>>()
    };
    for (req, target) in requests.iter().zip(targets) {
        if let Some(i) = index.get(&target).copied() {
            attach_request(
                &mut agents[i],
                str_at(req, "kind"),
                truthy_str(req, "created_at").as_deref(),
                req["async_input"].as_i64() == Some(1),
            );
        }
    }
    let known: HashSet<String> = agents.iter().map(|a| a.id.clone()).collect();
    for a in agents.iter_mut() {
        finish_agent_body(a, &known);
    }
    drop(content);
    let _cycles = timing::phase("structure_cycles");
    break_cycles(agents);
}

pub(crate) fn live_status(status: &str) -> bool {
    matches!(
        status,
        "running" | "starting" | "preparing" | "queued" | "waiting" | "pending" | "active"
    )
}

pub(crate) fn decorate(
    agents: &mut [Agent],
    subs: &[Json],
    nodes: &HashMap<String, Json>,
    events: &ItemRows<'_>,
    history: &[Json],
    spans: &[Json],
    plans: &[Json],
    path: &Path,
) -> Result<(), SourceError> {
    let _content = timing::phase("agent_content_decorate");
    let index: HashMap<String, usize> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| (a.id.clone(), i))
        .collect();
    let event_targets = {
        let attr = Attributor::new(agents, subs, nodes);
        match events {
            ItemRows::Json(rows) => rows
                .iter()
                .map(|row| attr.target(row).to_owned())
                .collect::<Vec<_>>(),
            ItemRows::Typed(rows) => rows
                .iter()
                .map(|row| attr.item_target(row).to_owned())
                .collect(),
        }
    };
    let count = match events {
        ItemRows::Json(rows) => rows.len(),
        ItemRows::Typed(rows) => rows.len(),
    };
    for (n, target) in (0..count).zip(event_targets) {
        let (failed, updated, started, kind, tool) = match events {
            ItemRows::Json(rows) => {
                let row = &rows[n];
                (
                    str_at(row, "status") == "failed",
                    row.get("updated_at").and_then(Value::as_str),
                    row.get("started_at").and_then(Value::as_str),
                    str_at(row, "type"),
                    row.get("tool").and_then(Value::as_str),
                )
            }
            ItemRows::Typed(rows) => {
                let row = rows[n];
                (
                    row.status == "failed",
                    Some(row.updated_at.as_str()),
                    row.started_at.as_deref(),
                    row.kind.as_str(),
                    row.tool.as_deref(),
                )
            }
        };
        let time = model::ms(if failed { updated } else { started }).or_else(|| model::ms(updated));
        if let (Some(i), Some(t)) = (index.get(&target).copied(), time) {
            let failed_item = if failed {
                Some(match events {
                    ItemRows::Json(rows) => serde_json::from_value(rows[n].clone())
                        .map_err(|e| problem(path, "schema", e.to_string()))?,
                    ItemRows::Typed(rows) => rows[n].clone(),
                })
            } else {
                None
            };
            attach_event(&mut agents[i], t, kind, tool, failed, failed_item.as_ref());
        }
    }
    let history_targets = {
        let attr = Attributor::new(agents, subs, nodes);
        history
            .iter()
            .map(|r| attr.target(r).to_owned())
            .collect::<Vec<_>>()
    };
    for (row, target) in history.iter().zip(history_targets) {
        let a = index.get(&target).copied();
        let used = number(row, "used");
        let max = number(row, "max");
        if let Some(i) = a {
            attach_history(
                &mut agents[i],
                used,
                max,
                model::ms(row.get("started_at").and_then(Value::as_str))
                    .or_else(|| model::ms(row.get("completed_at").and_then(Value::as_str)))
                    .unwrap_or(0.),
                str_at(row, "provider_thread_id"),
                optional(row, "model").as_deref().unwrap_or(""),
            );
        }
    }
    for row in spans {
        if let Some(i) = index.get(str_at(row, "thread_id")).copied() {
            attach_span(
                &mut agents[i],
                row.get("started_at").and_then(Value::as_str),
                row.get("requested_at").and_then(Value::as_str),
                row.get("completed_at").and_then(Value::as_str),
            );
        }
    }
    for a in agents.iter_mut() {
        finish_agent_tail(a);
    }
    let plan_targets = {
        let attr = Attributor::new(agents, subs, nodes);
        plans
            .iter()
            .map(|row| attr.target(row).to_owned())
            .collect::<Vec<_>>()
    };
    for (row, target) in plans.iter().zip(plan_targets) {
        if let Some(i) = index.get(&target).copied() {
            consider_plan(
                &mut agents[i],
                row.get("run_id").and_then(Value::as_str),
                row.get("steps").and_then(Value::as_str),
            );
        }
    }
    Ok(())
}
