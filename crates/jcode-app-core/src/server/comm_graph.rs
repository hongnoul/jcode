//! Server handlers for the task-DAG mutation ops (seed/expand/complete/inject).
//!
//! These are the live counterparts of the validated engine ops in
//! `jcode_plan::dag`. Each handler lifts the swarm's current `VersionedPlan` into
//! a `TaskGraph` (via `jcode_plan::bridge`), applies the engine op (which enforces
//! acyclicity, ownership, gate insertion, and artifact validation), lowers the
//! result back into the plan, then persists and broadcasts using the existing
//! swarm machinery. This keeps a single source of truth and reuses the scheduler,
//! persistence, and TUI broadcast paths.

use super::{
    SwarmEvent, SwarmEventType, SwarmMember, SwarmState, VersionedPlan, broadcast_swarm_plan,
    persist_swarm_state_for, record_swarm_event,
};
use crate::protocol::ServerEvent;
use crate::protocol::TaskGraphNodeSpec;
use jcode_plan::MAX_PLAN_ITEMS;
use jcode_plan::bridge::{apply_task_graph, parse_kind, to_task_graph};
use jcode_plan::dag::{self, HandoffArtifact, NodeKind, NodeSpec, NodeStatus, TaskGraph};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::{RwLock, broadcast};

fn spec_from_wire(spec: TaskGraphNodeSpec) -> NodeSpec {
    NodeSpec {
        id: Some(spec.id),
        content: spec.content,
        kind: parse_kind(spec.kind.as_deref()),
        depends_on: spec.depends_on,
        priority: spec.priority,
    }
}

#[derive(Debug, Clone, Copy)]
struct GraphGrowthConfig {
    soft_limit: usize,
    hard_limit: usize,
    max_fanout: usize,
    max_depth: usize,
}

impl GraphGrowthConfig {
    fn current() -> Self {
        let cfg = &crate::config::config().agents;
        Self {
            soft_limit: cfg.swarm_graph_soft_limit.max(1),
            hard_limit: cfg.swarm_graph_hard_limit.clamp(1, MAX_PLAN_ITEMS),
            max_fanout: cfg.swarm_graph_max_fanout.max(1),
            max_depth: cfg.swarm_graph_max_depth.max(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphGrowthHealth {
    total: usize,
    soft_limit: usize,
    adaptive_limit: usize,
    hard_limit: usize,
    completed: usize,
    useful_credits: usize,
    grown: usize,
    max_depth: usize,
}

fn useful_completion_credit(kind: NodeKind) -> usize {
    match kind {
        NodeKind::Implement | NodeKind::Fix => 8,
        NodeKind::Verify => 4,
        NodeKind::Synthesize => 2,
        NodeKind::Explore => 1,
        NodeKind::Critique => 0,
    }
}

fn node_depth(graph: &TaskGraph, node_id: &str) -> usize {
    let mut depth = 0usize;
    let mut cursor = graph.get(node_id).and_then(|node| node.parent.as_deref());
    // A cycle is rejected by the DAG engine. The bound is defensive for legacy
    // or corrupted persisted state and keeps diagnostics total.
    while let Some(parent) = cursor {
        depth += 1;
        if depth > graph.len() {
            break;
        }
        cursor = graph.get(parent).and_then(|node| node.parent.as_deref());
    }
    depth
}

fn graph_growth_health(graph: &TaskGraph, cfg: GraphGrowthConfig) -> GraphGrowthHealth {
    let completed = graph.nodes().iter().filter(|node| node.is_done()).count();
    let useful_credits = graph
        .nodes()
        .iter()
        .filter(|node| node.is_done() && !node.is_gate)
        .map(|node| useful_completion_credit(node.kind))
        .sum::<usize>();
    let grown = graph
        .nodes()
        .iter()
        .filter(|node| {
            node.origin
                .is_some_and(|origin| origin != jcode_plan::dag::NodeOrigin::Seed)
        })
        .count();
    let max_depth = graph
        .nodes()
        .iter()
        .map(|node| node_depth(graph, &node.id))
        .max()
        .unwrap_or(0);
    GraphGrowthHealth {
        total: graph.len(),
        soft_limit: cfg.soft_limit,
        adaptive_limit: cfg
            .soft_limit
            .saturating_add(useful_credits)
            .min(cfg.hard_limit),
        hard_limit: cfg.hard_limit,
        completed,
        useful_credits,
        grown,
        max_depth,
    }
}

fn normalized_task(content: &str) -> String {
    content
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn duplicate_new_nodes(before: &TaskGraph, after: &TaskGraph) -> usize {
    let existing_ids = before
        .nodes()
        .iter()
        .map(|node| node.id.as_str())
        .collect::<HashSet<_>>();
    let mut seen = before
        .nodes()
        .iter()
        .map(|node| normalized_task(&node.content))
        .collect::<HashSet<_>>();
    let mut duplicates = 0;
    for node in after
        .nodes()
        .iter()
        .filter(|node| !existing_ids.contains(node.id.as_str()))
    {
        if !seen.insert(normalized_task(&node.content)) {
            duplicates += 1;
        }
    }
    duplicates
}

/// Admission policy for machinery-grown deep graphs. Independent seed batches
/// may use the configured hard ceiling directly; recursive expansion must earn
/// capacity by completing useful work. This is intentional hysteresis: every
/// accepted completion permanently raises the current plan's budget, while a
/// stalled audit tree cannot oscillate itself back into an allowed state.
fn graph_growth_error(
    before: &TaskGraph,
    after: &TaskGraph,
    requested_fanout: usize,
    is_seed: bool,
    cfg: GraphGrowthConfig,
) -> Option<String> {
    let health = graph_growth_health(after, cfg);
    if health.total > MAX_PLAN_ITEMS {
        return Some(format!(
            "plan would contain {} items, exceeding Jcode's absolute emergency ceiling of {}",
            health.total, MAX_PLAN_ITEMS
        ));
    }
    if health.total > health.hard_limit {
        return Some(format!(
            "plan would contain {} items, exceeding configured agents.swarm_graph_hard_limit={}; raise that setting only when the workload is genuinely independent",
            health.total, health.hard_limit
        ));
    }
    if is_seed || !matches!(after.mode, jcode_plan::dag::Mode::Deep) {
        return None;
    }
    if requested_fanout > cfg.max_fanout {
        return Some(format!(
            "adaptive growth throttled: requested fan-out {requested_fanout} exceeds agents.swarm_graph_max_fanout={}; split only after the current wave completes",
            cfg.max_fanout
        ));
    }
    if health.max_depth > cfg.max_depth {
        return Some(format!(
            "adaptive growth throttled: recursive depth {} exceeds agents.swarm_graph_max_depth={}; finish or flatten existing work instead of decomposing again",
            health.max_depth, cfg.max_depth
        ));
    }
    if health.total > health.soft_limit {
        let new_count = after.len().saturating_sub(before.len());
        let duplicate_count = duplicate_new_nodes(before, after);
        if new_count >= 2 && duplicate_count * 2 >= new_count {
            return Some(format!(
                "adaptive growth throttled: {duplicate_count} of {new_count} new nodes duplicate existing task descriptions; reuse or complete existing nodes"
            ));
        }
        if health.total > health.adaptive_limit {
            return Some(format!(
                "adaptive growth throttled: graph health total={} grown={} completed={} useful_credits={} allows {} nodes (soft={} hard={}); complete implementation/fix/verification work to earn capacity before expanding again",
                health.total,
                health.grown,
                health.completed,
                health.useful_credits,
                health.adaptive_limit,
                health.soft_limit,
                health.hard_limit,
            ));
        }
    }
    None
}

async fn swarm_id_for(
    session_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Option<String> {
    swarm_members
        .read()
        .await
        .get(session_id)
        .and_then(|member| member.swarm_id.clone())
}

/// Ensure the seeding session can actually drive the graph it just created.
///
/// Deep-mode sessions are frequently solo `agent`s with no coordinator elected,
/// yet `assign_task` / `assign_next` / `run_plan` are coordinator-gated. Without
/// this, a fresh deep-mode agent can seed a task graph but then cannot dispatch
/// any of it. We elect the seeder as coordinator when the swarm has no *live*
/// coordinator, mirroring the self-promote rule used by `assign_role`. A live,
/// non-headless coordinator is left untouched so a real coordinator is never
/// displaced by a worker that happens to seed.
///
/// Returns true when the seeder was (or already is) the coordinator afterwards.
async fn ensure_seeder_can_coordinate(
    swarm_id: &str,
    seeder_session_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
) -> bool {
    // 1. Read the current coordinator id without holding the lock across the
    //    liveness check (matches the non-nested lock pattern used elsewhere).
    let current = swarm_coordinators.read().await.get(swarm_id).cloned();
    match &current {
        Some(coord) if coord == seeder_session_id => return true,
        _ => {}
    }

    // 2. Decide whether the existing coordinator is still a live driver.
    let coordinator_is_live = match &current {
        Some(coord) => {
            let members = swarm_members.read().await;
            members
                .get(coord)
                .map(|member| !member.event_tx.is_closed() && !member.is_headless)
                .unwrap_or(false)
        }
        None => false,
    };
    if coordinator_is_live {
        return false;
    }

    // 3. Promote the seeder; demote any prior (stale) coordinator member. Re-check
    //    under the write lock that the coordinator is still the one we inspected
    //    (compare-and-swap): two concurrent seeders race here, and the loser must
    //    not silently displace the winner it never liveness-checked.
    let prior = {
        let mut coordinators = swarm_coordinators.write().await;
        if coordinators.get(swarm_id) != current.as_ref() {
            // Someone else changed the coordinator between our read and write.
            return coordinators.get(swarm_id).map(String::as_str) == Some(seeder_session_id);
        }
        coordinators.insert(swarm_id.to_string(), seeder_session_id.to_string())
    };
    {
        let mut members = swarm_members.write().await;
        if let Some(member) = members.get_mut(seeder_session_id) {
            member.role = "coordinator".to_string();
        }
        if let Some(prior) = prior
            && prior != seeder_session_id
            && let Some(member) = members.get_mut(&prior)
        {
            member.role = "agent".to_string();
        }
    }
    true
}

/// Auto-claim a queued node for the participant that is trying to mutate it.
///
/// Seeded nodes are unowned until dispatch, but the deep-mode contract tells the
/// seeding agent to `expand_node`/`complete_node` its own nodes, and the assign
/// path refuses self-assignment — so without this a solo deep seeder could never
/// legally touch any node it seeded (observed live as "Complete rejected: actor
/// does not own node"). Similarly, assignment to a client-attached worker leaves
/// the item `queued` (the server-run flip to `running` is skipped when a live
/// client owns the turn), so the assignee's own complete/expand would bounce with
/// "invalid state Queued".
///
/// Claiming is safe only when the node is genuinely available to this actor:
/// queued, with every dependency done (enforced by `dispatch`), and either
/// unowned or already assigned to this same actor. A node owned by someone else
/// is never touched — the engine's `NotOwner` check still applies.
fn claim_queued_node_for_actor(graph: &mut TaskGraph, node_id: &str, actor: &str) {
    let claimable = graph.get(node_id).is_some_and(|node| {
        node.status == NodeStatus::Queued
            && node.owner.as_deref().is_none_or(|owner| owner == actor)
    });
    if claimable {
        // `dispatch` re-validates queued status and dependency satisfaction; if
        // deps are not done the claim is skipped and the engine op reports the
        // real error.
        let _ = dag::dispatch(graph, node_id, actor);
    }
}

fn err(client_event_tx: &mpsc::UnboundedSender<ServerEvent>, id: u64, message: String) {
    let _ = client_event_tx.send(ServerEvent::Error {
        id,
        message,
        retry_after_secs: None,
    });
}

/// Shared finalize: persist, broadcast, record a plan-update event, and ack.
#[expect(
    clippy::too_many_arguments,
    reason = "finalize threads through swarm persistence, broadcast, and event-history handles"
)]
async fn finalize(
    id: u64,
    swarm_id: &str,
    req_session_id: &str,
    reason: &str,
    item_count: usize,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
) {
    let from_name = swarm_members
        .read()
        .await
        .get(req_session_id)
        .and_then(|member| member.friendly_name.clone());

    let swarm_state = SwarmState {
        members: Arc::clone(swarm_members),
        swarms_by_id: Arc::clone(swarms_by_id),
        plans: Arc::clone(swarm_plans),
        coordinators: Arc::clone(swarm_coordinators),
    };
    persist_swarm_state_for(swarm_id, &swarm_state).await;
    broadcast_swarm_plan(
        swarm_id,
        Some(reason.to_string()),
        swarm_plans,
        swarm_members,
        swarms_by_id,
    )
    .await;
    record_swarm_event(
        event_history,
        event_counter,
        swarm_event_tx,
        req_session_id.to_string(),
        from_name,
        Some(swarm_id.to_string()),
        SwarmEventType::PlanUpdate {
            swarm_id: swarm_id.to_string(),
            item_count,
        },
    )
    .await;
    let _ = client_event_tx.send(ServerEvent::Done { id });
}

/// Seed (or re-seed) the swarm task DAG from a batch of node specs.
#[expect(
    clippy::too_many_arguments,
    reason = "swarm op threads runtime handles"
)]
pub(super) async fn handle_comm_seed_graph(
    id: u64,
    req_session_id: String,
    mode: Option<String>,
    nodes: Vec<TaskGraphNodeSpec>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
) {
    let Some(swarm_id) = swarm_id_for(&req_session_id, swarm_members).await else {
        err(client_event_tx, id, "Not in a swarm.".to_string());
        return;
    };

    // A deep-mode seeder is usually a solo agent. Elect it coordinator (when no
    // live coordinator exists) so it can actually dispatch the graph it seeds via
    // the coordinator-gated assign/run_plan paths.
    ensure_seeder_can_coordinate(
        &swarm_id,
        &req_session_id,
        swarm_members,
        swarm_coordinators,
    )
    .await;

    let specs: Vec<NodeSpec> = nodes.into_iter().map(spec_from_wire).collect();
    let count = specs.len();

    // Resolve the plan mode. The model is *asked* to pass `mode:"deep"` when it is
    // running at `swarm-deep` effort, but it frequently forgets. Rather than
    // silently downgrading a deep-effort session to light (which disables the
    // gates + artifact validation that define deep mode), default the mode from
    // the seeder's recorded reasoning effort when the caller did not specify one.
    // An explicit `mode` always wins so a caller can still opt into light.
    let resolved_mode = mode.or_else(|| {
        crate::session_effort::session_effort(&req_session_id)
            .filter(|effort| crate::prompt::is_deep_swarm_effort(effort))
            .map(|_| "deep".to_string())
    });

    let result = {
        let mut plans = swarm_plans.write().await;
        let plan = plans
            .entry(swarm_id.clone())
            .or_insert_with(VersionedPlan::new);
        if let Some(mode) = resolved_mode {
            // Guard against silent rigor downgrades: re-seeding an existing deep
            // plan as light would strip the gates + artifact validation from all
            // nodes already in flight. Deepening (light -> deep) or re-stating
            // the same mode is fine; only the downgrade of a non-empty deep plan
            // is rejected.
            let downgrades_deep = plan.mode.eq_ignore_ascii_case("deep")
                && !mode.eq_ignore_ascii_case("deep")
                && !plan.items.is_empty();
            if downgrades_deep {
                err(
                    client_event_tx,
                    id,
                    "Seed rejected: this swarm already has a non-empty deep-mode plan; \
                     seeding with mode=light would silently strip its gates and artifact \
                     validation. Omit `mode` to keep deep, or finish/clear the current plan first."
                        .to_string(),
                );
                return;
            }
            plan.mode = mode;
        }
        plan.participants.insert(req_session_id.clone());
        let mut graph = to_task_graph(plan);
        let before = graph.clone();
        match dag::seed(&mut graph, specs) {
            Ok(()) => {
                match graph_growth_error(&before, &graph, count, true, GraphGrowthConfig::current())
                {
                    Some(message) => Err(message),
                    None => {
                        if graph != before {
                            apply_task_graph(plan, &graph);
                            plan.version += 1;
                        }
                        Ok(())
                    }
                }
            }
            Err(e) => Err(e.to_string()),
        }
    };

    match result {
        Ok(()) => {
            finalize(
                id,
                &swarm_id,
                &req_session_id,
                "task_graph_seed",
                count,
                client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Err(e) => err(client_event_tx, id, format!("Seed rejected: {e}")),
    }
}

/// Decompose a node the caller owns into a child sub-DAG.
#[expect(
    clippy::too_many_arguments,
    reason = "swarm op threads runtime handles"
)]
pub(super) async fn handle_comm_expand_node(
    id: u64,
    req_session_id: String,
    node_id: String,
    children: Vec<TaskGraphNodeSpec>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
) {
    let Some(swarm_id) = swarm_id_for(&req_session_id, swarm_members).await else {
        err(client_event_tx, id, "Not in a swarm.".to_string());
        return;
    };
    let specs: Vec<NodeSpec> = children.into_iter().map(spec_from_wire).collect();
    let count = specs.len();

    let result = {
        let mut plans = swarm_plans.write().await;
        let Some(plan) = plans.get_mut(&swarm_id) else {
            err(client_event_tx, id, "No plan for this swarm.".to_string());
            return;
        };
        let mut graph = to_task_graph(plan);
        let before = graph.clone();
        claim_queued_node_for_actor(&mut graph, &node_id, &req_session_id);
        match dag::expand_node(&mut graph, &node_id, &req_session_id, specs) {
            Ok(_) => match graph_growth_error(
                &before,
                &graph,
                count,
                false,
                GraphGrowthConfig::current(),
            ) {
                Some(message) => Err(message),
                None => {
                    apply_task_graph(plan, &graph);
                    plan.version += 1;
                    Ok(())
                }
            },
            Err(e) => Err(e.to_string()),
        }
    };

    match result {
        Ok(()) => {
            finalize(
                id,
                &swarm_id,
                &req_session_id,
                "task_graph_expand",
                count,
                client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Err(e) => err(client_event_tx, id, format!("Expand rejected: {e}")),
    }
}

/// Complete a node the caller owns with a typed handoff artifact.
#[expect(
    clippy::too_many_arguments,
    reason = "swarm op threads runtime handles"
)]
pub(super) async fn handle_comm_complete_node(
    id: u64,
    req_session_id: String,
    node_id: String,
    artifact_json: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
) {
    let Some(swarm_id) = swarm_id_for(&req_session_id, swarm_members).await else {
        err(client_event_tx, id, "Not in a swarm.".to_string());
        return;
    };

    let artifact: HandoffArtifact = match serde_json::from_str(&artifact_json) {
        Ok(artifact) => artifact,
        Err(e) => {
            err(client_event_tx, id, format!("Invalid artifact JSON: {e}"));
            return;
        }
    };

    let result = {
        let mut plans = swarm_plans.write().await;
        let Some(plan) = plans.get_mut(&swarm_id) else {
            err(client_event_tx, id, "No plan for this swarm.".to_string());
            return;
        };
        let mut graph = to_task_graph(plan);
        claim_queued_node_for_actor(&mut graph, &node_id, &req_session_id);
        match dag::complete_node(&mut graph, &node_id, &req_session_id, artifact) {
            Ok(()) => {
                apply_task_graph(plan, &graph);
                plan.version += 1;
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    };

    match result {
        Ok(()) => {
            finalize(
                id,
                &swarm_id,
                &req_session_id,
                "task_graph_complete",
                1,
                client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Err(e) => err(client_event_tx, id, format!("Complete rejected: {e}")),
    }
}

/// Inject gap/fix nodes from a gate the caller owns.
#[expect(
    clippy::too_many_arguments,
    reason = "swarm op threads runtime handles"
)]
pub(super) async fn handle_comm_inject_gap(
    id: u64,
    req_session_id: String,
    gate_id: String,
    nodes: Vec<TaskGraphNodeSpec>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
) {
    let Some(swarm_id) = swarm_id_for(&req_session_id, swarm_members).await else {
        err(client_event_tx, id, "Not in a swarm.".to_string());
        return;
    };
    let specs: Vec<NodeSpec> = nodes.into_iter().map(spec_from_wire).collect();
    let count = specs.len();

    let result = {
        let mut plans = swarm_plans.write().await;
        let Some(plan) = plans.get_mut(&swarm_id) else {
            err(client_event_tx, id, "No plan for this swarm.".to_string());
            return;
        };
        let mut graph = to_task_graph(plan);
        let before = graph.clone();
        claim_queued_node_for_actor(&mut graph, &gate_id, &req_session_id);
        match dag::inject_from_gate(&mut graph, &gate_id, &req_session_id, specs) {
            Ok(_) => match graph_growth_error(
                &before,
                &graph,
                count,
                false,
                GraphGrowthConfig::current(),
            ) {
                Some(message) => Err(message),
                None => {
                    apply_task_graph(plan, &graph);
                    plan.version += 1;
                    Ok(())
                }
            },
            Err(e) => Err(e.to_string()),
        }
    };

    match result {
        Ok(()) => {
            finalize(
                id,
                &swarm_id,
                &req_session_id,
                "task_graph_inject_gap",
                count,
                client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Err(e) => err(client_event_tx, id, format!("Inject rejected: {e}")),
    }
}

#[cfg(test)]
mod growth_tests {
    use super::*;
    use jcode_plan::dag::{Mode, NodeOrigin, TaskNode};

    fn cfg() -> GraphGrowthConfig {
        GraphGrowthConfig {
            soft_limit: 4,
            hard_limit: 32,
            max_fanout: 8,
            max_depth: 4,
        }
    }

    fn node(id: &str, kind: NodeKind, status: NodeStatus, origin: NodeOrigin) -> TaskNode {
        TaskNode {
            id: id.to_string(),
            content: format!("{kind:?} task {id}"),
            kind,
            status,
            owner: None,
            parent: None,
            depends_on: Vec::new(),
            expanded: false,
            is_gate: matches!(kind, NodeKind::Critique),
            planner: None,
            priority: 100,
            output: None,
            origin: Some(origin),
        }
    }

    fn graph(nodes: Vec<TaskNode>) -> TaskGraph {
        let mut graph = TaskGraph::new(Mode::Deep);
        for node in nodes {
            graph.push_node(node);
        }
        graph
    }

    #[test]
    fn recursive_audit_growth_stalls_without_useful_completions() {
        let before = graph(
            (0..4)
                .map(|i| {
                    node(
                        &format!("seed-{i}"),
                        NodeKind::Explore,
                        NodeStatus::Queued,
                        NodeOrigin::Seed,
                    )
                })
                .collect(),
        );
        let mut after = before.clone();
        after.push_node(node(
            "audit-a",
            NodeKind::Explore,
            NodeStatus::Queued,
            NodeOrigin::Expand,
        ));
        after.push_node(node(
            "audit-b",
            NodeKind::Critique,
            NodeStatus::Queued,
            NodeOrigin::Gate,
        ));

        let error = graph_growth_error(&before, &after, 2, false, cfg())
            .expect("unproductive recursive growth should throttle");
        assert!(error.contains("useful_credits=0"), "{error}");
        assert!(error.contains("allows 4 nodes"), "{error}");
    }

    #[test]
    fn implementation_completions_earn_capacity_past_soft_limit() {
        let before = graph(vec![
            node(
                "impl-a",
                NodeKind::Implement,
                NodeStatus::Done,
                NodeOrigin::Seed,
            ),
            node("impl-b", NodeKind::Fix, NodeStatus::Done, NodeOrigin::Seed),
            node(
                "seed-c",
                NodeKind::Explore,
                NodeStatus::Queued,
                NodeOrigin::Seed,
            ),
            node(
                "seed-d",
                NodeKind::Explore,
                NodeStatus::Queued,
                NodeOrigin::Seed,
            ),
        ]);
        let mut after = before.clone();
        for i in 0..8 {
            after.push_node(node(
                &format!("productive-{i}"),
                NodeKind::Implement,
                NodeStatus::Queued,
                NodeOrigin::Expand,
            ));
        }

        assert_eq!(graph_growth_health(&after, cfg()).adaptive_limit, 20);
        assert!(graph_growth_error(&before, &after, 8, false, cfg()).is_none());
    }

    #[test]
    fn large_independent_seed_can_exceed_soft_limit() {
        let before = graph(Vec::new());
        let after = graph(
            (0..20)
                .map(|i| {
                    node(
                        &format!("independent-{i}"),
                        NodeKind::Implement,
                        NodeStatus::Queued,
                        NodeOrigin::Seed,
                    )
                })
                .collect(),
        );

        assert!(graph_growth_error(&before, &after, 20, true, cfg()).is_none());
    }

    #[test]
    fn duplicate_recursive_wave_is_rejected_even_with_credits() {
        let before = graph(vec![
            node(
                "impl",
                NodeKind::Implement,
                NodeStatus::Done,
                NodeOrigin::Seed,
            ),
            node("a", NodeKind::Explore, NodeStatus::Queued, NodeOrigin::Seed),
            node("b", NodeKind::Explore, NodeStatus::Queued, NodeOrigin::Seed),
            node("c", NodeKind::Explore, NodeStatus::Queued, NodeOrigin::Seed),
        ]);
        let mut after = before.clone();
        let mut first = node(
            "dup-1",
            NodeKind::Explore,
            NodeStatus::Queued,
            NodeOrigin::Expand,
        );
        first.content = "audit the same subsystem".to_string();
        let mut second = node(
            "dup-2",
            NodeKind::Explore,
            NodeStatus::Queued,
            NodeOrigin::Expand,
        );
        second.content = "Audit, the same subsystem!".to_string();
        after.push_node(first);
        after.push_node(second);

        let error = graph_growth_error(&before, &after, 2, false, cfg())
            .expect("duplicate wave should throttle");
        assert!(
            error.contains("duplicate existing task descriptions"),
            "{error}"
        );
    }
}
