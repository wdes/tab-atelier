// SPDX-License-Identifier: MPL-2.0

//! The fleet as a graph: who is working on what, and where.
//!
//! Three things know pieces of this; none knows the whole.
//!
//! The
//! blackboard knows what work exists and who claimed it, the lease registry
//! knows what is actually held right now, and the tab list knows which agents
//! are alive. A renderer shouldn't have to join those itself, so this joins
//! them into nodes and edges.
//!
//! Node kinds: `host`, `agent`, `task`. Edge kinds:
//!
//! | edge        | from → to     | means                                    |
//! |-------------|---------------|------------------------------------------|
//! | `runs_on`   | agent → host  | this tab lives on this machine           |
//! | `announced` | agent → task  | put the work on the board                |
//! | `bid`       | agent → task  | offered to do it, at `label` cost        |
//! | `works_on`  | agent → task  | awarded it — the edge a viewer cares about |
//! | `home`      | task → host   | whose lease table is authoritative       |
//! | `peer`      | host → host   | a federation link we have learned        |
//!
//! `works_on` carries whether a **live lease** backs it. An award with no
//! lease is the interesting case: the agent said it was working and then died,
//! or its lease lapsed while it was thinking. That is exactly what a human
//! staring at the graph wants to spot, so it is a field rather than something
//! the renderer has to infer.

use serde::{Deserialize, Serialize};

/// One node in the fleet graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Unique within the graph: `host:<name>`, `agent:<name>`, `task:<id>`.
    pub id: String,
    /// `host` / `agent` / `task`.
    pub kind: String,
    /// What to draw on it.
    pub label: String,
    /// Task lifecycle (`open`/`bidding`/`awarded`/`done`/`failed`) or an
    /// agent's reported state. Absent on hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// For agents, the tab uuid, so a viewer can link to the tab itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
}

/// One directed edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// On `works_on`: whether a live lease backs the award. False means the
    /// agent claimed the work and then stopped holding it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leased: Option<bool>,
    /// On `works_on`: milliseconds until the lease lapses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_ms: Option<u64>,
}

/// Nodes + edges, ready to render.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// A live agent, as the tab list sees it.
#[derive(Debug, Clone, Default)]
pub struct AgentTab {
    pub name: String,
    pub uuid: String,
    pub state: Option<String>,
}

fn agent_id(name: &str) -> String {
    format!("agent:{name}")
}

/// Join board, leases and tabs into a graph.
///
/// Pure so the shape is testable without a daemon: the route is a thin wrapper
/// that gathers the three inputs.
#[must_use]
pub fn build(
    board: &[crate::cli::tasks::TaskView],
    claims: &[crate::claims::Claim],
    tabs: &[AgentTab],
    me: &str,
    peers: &std::collections::BTreeMap<String, String>,
    now_ms: u64,
) -> Graph {
    let mut g = Graph::default();
    let mut have: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    let push_node = |g: &mut Graph, have: &mut std::collections::BTreeSet<String>, n: Node| {
        if have.insert(n.id.clone()) {
            g.nodes.push(n);
        }
    };

    // Hosts: us, every peer we have learned, and every host some task calls
    // home — a task whose home we can't reach is still worth drawing.
    push_node(
        &mut g,
        &mut have,
        Node {
            id: format!("host:{me}"),
            kind: "host".into(),
            label: me.to_owned(),
            status: Some("self".into()),
            uuid: None,
        },
    );
    for peer in peers.keys() {
        push_node(
            &mut g,
            &mut have,
            Node {
                id: format!("host:{peer}"),
                kind: "host".into(),
                label: peer.clone(),
                status: Some("peer".into()),
                uuid: None,
            },
        );
        g.edges.push(Edge {
            from: format!("host:{me}"),
            to: format!("host:{peer}"),
            kind: "peer".into(),
            label: None,
            leased: None,
            expires_in_ms: None,
        });
    }

    // Agents that are actually running here.
    for t in tabs {
        push_node(
            &mut g,
            &mut have,
            Node {
                id: agent_id(&t.name),
                kind: "agent".into(),
                label: t.name.clone(),
                status: t.state.clone(),
                uuid: Some(t.uuid.clone()),
            },
        );
        g.edges.push(Edge {
            from: agent_id(&t.name),
            to: format!("host:{me}"),
            kind: "runs_on".into(),
            label: None,
            leased: None,
            expires_in_ms: None,
        });
    }

    let live: std::collections::BTreeMap<&str, &crate::claims::Claim> =
        claims.iter().map(|c| (c.key.as_str(), c)).collect();

    for t in board {
        let task_node = format!("task:{}", t.id);
        push_node(
            &mut g,
            &mut have,
            Node {
                id: task_node.clone(),
                kind: "task".into(),
                label: if t.title.is_empty() {
                    t.id.clone()
                } else {
                    t.title.clone()
                },
                status: Some(t.state().as_str().to_owned()),
                uuid: None,
            },
        );
        if let Some(home) = &t.home {
            push_node(
                &mut g,
                &mut have,
                Node {
                    id: format!("host:{home}"),
                    kind: "host".into(),
                    label: home.clone(),
                    status: Some("peer".into()),
                    uuid: None,
                },
            );
            g.edges.push(Edge {
                from: task_node.clone(),
                to: format!("host:{home}"),
                kind: "home".into(),
                label: None,
                leased: None,
                expires_in_ms: None,
            });
        }
        // An agent named on the board may have no tab here — it can be running
        // on another host. Draw it anyway, or half the graph goes missing.
        let ensure_agent = |g: &mut Graph, have: &mut std::collections::BTreeSet<String>, name: &str| {
            if have.insert(agent_id(name)) {
                g.nodes.push(Node {
                    id: agent_id(name),
                    kind: "agent".into(),
                    label: name.to_owned(),
                    status: Some("elsewhere".into()),
                    uuid: None,
                });
            }
        };
        if let Some(who) = &t.announced_by {
            ensure_agent(&mut g, &mut have, who);
            g.edges.push(Edge {
                from: agent_id(who),
                to: task_node.clone(),
                kind: "announced".into(),
                label: None,
                leased: None,
                expires_in_ms: None,
            });
        }
        for (bidder, cost) in &t.bids {
            ensure_agent(&mut g, &mut have, bidder);
            g.edges.push(Edge {
                from: agent_id(bidder),
                to: task_node.clone(),
                kind: "bid".into(),
                label: Some(cost.to_string()),
                leased: None,
                expires_in_ms: None,
            });
        }
        if let Some(who) = &t.awarded_to {
            ensure_agent(&mut g, &mut have, who);
            let claim = live.get(t.claim_key().as_str()).copied();
            g.edges.push(Edge {
                from: agent_id(who),
                to: task_node,
                kind: "works_on".into(),
                label: None,
                // An award with no live lease means the agent stopped holding
                // the work — died mid-task, or thought past its expiry. That is
                // the thing a human scanning the graph wants to see.
                leased: Some(claim.is_some_and(|c| c.holder == *who)),
                expires_in_ms: claim.map(|c| c.remaining_ms(now_ms)),
            });
        }
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::tasks::TaskView;

    fn task(id: &str, awarded: Option<&str>, done: Option<bool>) -> TaskView {
        TaskView {
            id: id.into(),
            title: format!("do {id}"),
            announced_by: Some("backlog".into()),
            home: Some("host-a".into()),
            announced_ts: 10,
            last_ts: 20,
            bids: Vec::new(),
            awarded_to: awarded.map(ToOwned::to_owned),
            done: done.map(|ok| (ok, String::new())),
        }
    }

    fn claim(key: &str, holder: &str, expires: u64) -> crate::claims::Claim {
        crate::claims::Claim {
            key: key.into(),
            holder: holder.into(),
            granted_ms: 0,
            expires_ms: expires,
            fence: 1,
        }
    }

    fn find<'a>(g: &'a Graph, kind: &str, from: &str) -> Option<&'a Edge> {
        g.edges.iter().find(|e| e.kind == kind && e.from == from)
    }

    #[test]
    fn the_graph_joins_board_leases_and_tabs() {
        let board = vec![task("cov:a", Some("agent-1"), None), task("cov:b", None, None)];
        let claims = vec![claim("task:cov:a", "agent-1", 60_000)];
        let tabs = vec![AgentTab {
            name: "agent-1".into(),
            uuid: "uuid-1".into(),
            state: Some("thinking".into()),
        }];
        let peers = std::collections::BTreeMap::from([("host-b".to_string(), "ep-1".to_string())]);
        let g = build(&board, &claims, &tabs, "host-a", &peers, 0);

        // One node per distinct thing, no duplicates even though host-a is
        // both us and the home of both tasks.
        let hosts: Vec<&str> = g
            .nodes
            .iter()
            .filter(|n| n.kind == "host")
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(hosts, vec!["host:host-a", "host:host-b"]);
        assert_eq!(g.nodes.iter().filter(|n| n.kind == "task").count(), 2);
        let agent = g.nodes.iter().find(|n| n.id == "agent:agent-1").expect("agent node");
        assert_eq!(agent.uuid.as_deref(), Some("uuid-1"), "a viewer can link to the tab");
        assert_eq!(agent.status.as_deref(), Some("thinking"));

        // The edge a viewer actually wants, backed by a live lease.
        let w = find(&g, "works_on", "agent:agent-1").expect("works_on edge");
        assert_eq!(w.to, "task:cov:a");
        assert_eq!(w.leased, Some(true));
        assert_eq!(w.expires_in_ms, Some(60_000));

        assert!(find(&g, "runs_on", "agent:agent-1").is_some());
        assert!(find(&g, "peer", "host:host-a").is_some());
        assert_eq!(
            find(&g, "home", "task:cov:a").map(|e| e.to.as_str()),
            Some("host:host-a")
        );
        // Unawarded work has no works_on edge at all.
        assert!(g.edges.iter().all(|e| e.kind != "works_on" || e.to != "task:cov:b"));
    }

    #[test]
    fn an_award_without_a_live_lease_is_visible_as_such() {
        // The failure a human is scanning for: the agent said it was working
        // and then stopped holding the lease — it died, or thought past its
        // expiry. The renderer must not have to infer this.
        let board = vec![task("cov:a", Some("agent-1"), None)];
        let g = build(&board, &[], &[], "host-a", &std::collections::BTreeMap::default(), 0);
        let w = find(&g, "works_on", "agent:agent-1").expect("works_on edge");
        assert_eq!(w.leased, Some(false));
        assert_eq!(w.expires_in_ms, None);

        // A lease held by somebody *else* doesn't count as backing this award.
        let claims = vec![claim("task:cov:a", "agent-2", 60_000)];
        let g = build(
            &board,
            &claims,
            &[],
            "host-a",
            &std::collections::BTreeMap::default(),
            0,
        );
        assert_eq!(
            find(&g, "works_on", "agent:agent-1").and_then(|e| e.leased),
            Some(false)
        );
    }

    #[test]
    fn agents_with_no_tab_here_are_still_drawn() {
        // Half the fleet is on another host. Omitting those agents would make
        // a federated graph look like work assigned to nobody.
        let board = vec![task("cov:a", Some("remote-agent"), None)];
        let g = build(&board, &[], &[], "host-a", &std::collections::BTreeMap::default(), 0);
        let n = g.nodes.iter().find(|n| n.id == "agent:remote-agent").expect("node");
        assert_eq!(n.status.as_deref(), Some("elsewhere"));
        assert!(n.uuid.is_none(), "we have no tab uuid for a remote agent");
        // …and it is not claimed to run here.
        assert!(
            g.edges
                .iter()
                .all(|e| e.kind != "runs_on" || e.from != "agent:remote-agent")
        );
    }

    #[test]
    fn bids_and_announcements_are_edges_too() {
        let mut t = task("cov:a", None, None);
        t.bids = vec![("agent-1".into(), 40), ("agent-2".into(), 90)];
        let g = build(&[t], &[], &[], "host-a", &std::collections::BTreeMap::default(), 0);
        assert_eq!(
            find(&g, "bid", "agent:agent-1").and_then(|e| e.label.clone()),
            Some("40".into())
        );
        assert_eq!(g.edges.iter().filter(|e| e.kind == "bid").count(), 2);
        assert_eq!(
            find(&g, "announced", "agent:backlog").map(|e| e.to.as_str()),
            Some("task:cov:a")
        );
        // An empty fleet is an empty graph plus the host we are, not an error.
        let empty = build(&[], &[], &[], "solo", &std::collections::BTreeMap::default(), 0);
        assert_eq!(empty.nodes.len(), 1);
        assert!(empty.edges.is_empty());
    }
}
