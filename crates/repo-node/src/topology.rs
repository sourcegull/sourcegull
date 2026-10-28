//! The repo node list and this process's identity in it.
//!
//! Nodes are identified by their gRPC address (`http://repo-0.repo:3178`): that string is what
//! the router hashes, what `repo.shard` persists and what web processes connect to, so every
//! process must list the nodes with the same spelling.

use std::net::SocketAddr;

use anyhow::bail;

/// The node list and, for a process running the `repo` role, its own address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topology {
    /// Every repo node, normalized, in configuration order.
    pub nodes: Vec<String>,
    /// This process's node address (only with the `repo` role).
    pub me: Option<String>,
}

/// Normalizes a node address: trims it and adds `http://` when no scheme is given.
pub fn normalize_addr(addr: &str) -> String {
    let addr = addr.trim().trim_end_matches('/');
    if addr.contains("://") {
        addr.to_owned()
    } else {
        format!("http://{addr}")
    }
}

/// The host part of a node address.
fn host_of(addr: &str) -> &str {
    let rest = addr.split_once("://").map_or(addr, |(_, r)| r);
    let authority = rest.split('/').next().unwrap_or(rest);
    match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => authority,
    }
}

/// Works out the topology from `SOURCEGULL_REPO_NODES` (`nodes`), `SOURCEGULL_REPO_NODE_ADDR`
/// (`explicit_me`) and the gRPC listen address.
///
/// With the `repo` role (`runs_repo`), this node's address is, in order: `explicit_me`; the only
/// listed node; the listed node whose host is `hostname` or starts with `hostname.` (a
/// StatefulSet pod `sourcegull-repo-1` listed as `http://sourcegull-repo-1.sourcegull-repo:3178`); or, when
/// no nodes are listed (single node), the loopback address of the gRPC listener. An
/// empty list then becomes just this node.
pub fn resolve(
    nodes: &[String],
    explicit_me: Option<&str>,
    grpc_addr: SocketAddr,
    runs_repo: bool,
    hostname: Option<&str>,
) -> anyhow::Result<Topology> {
    let mut list: Vec<String> = Vec::new();
    for n in nodes.iter().filter(|n| !n.trim().is_empty()) {
        let n = normalize_addr(n);
        if !list.contains(&n) {
            list.push(n);
        }
    }
    if !runs_repo {
        return Ok(Topology {
            nodes: list,
            me: None,
        });
    }
    let me = if let Some(me) = explicit_me.filter(|m| !m.trim().is_empty()) {
        normalize_addr(me)
    } else if list.len() == 1 {
        list[0].clone()
    } else if list.is_empty() {
        let ip = if grpc_addr.ip().is_unspecified() {
            match grpc_addr {
                SocketAddr::V4(_) => "127.0.0.1".to_owned(),
                SocketAddr::V6(_) => "[::1]".to_owned(),
            }
        } else {
            match grpc_addr {
                SocketAddr::V4(a) => a.ip().to_string(),
                SocketAddr::V6(a) => format!("[{}]", a.ip()),
            }
        };
        format!("http://{ip}:{}", grpc_addr.port())
    } else {
        let Some(host) = hostname.filter(|h| !h.is_empty()) else {
            bail!(
                "several repo nodes are listed in SOURCEGULL_REPO_NODES; set SOURCEGULL_REPO_NODE_ADDR to \
                 this node's address"
            );
        };
        let matches: Vec<&String> = list
            .iter()
            .filter(|n| {
                let h = host_of(n);
                h == host || h.starts_with(&format!("{host}."))
            })
            .collect();
        match matches.as_slice() {
            [one] => (*one).clone(),
            [] => bail!(
                "no entry of SOURCEGULL_REPO_NODES matches this host ({host}); set \
                 SOURCEGULL_REPO_NODE_ADDR"
            ),
            _ => bail!(
                "several entries of SOURCEGULL_REPO_NODES match this host ({host}); set \
                 SOURCEGULL_REPO_NODE_ADDR"
            ),
        }
    };
    if list.is_empty() {
        list.push(me.clone());
    } else if !list.contains(&me) {
        bail!("SOURCEGULL_REPO_NODE_ADDR {me:?} is not listed in SOURCEGULL_REPO_NODES");
    }
    Ok(Topology {
        nodes: list,
        me: Some(me),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "0.0.0.0:3178".parse().unwrap()
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn single_node_defaults_to_loopback() {
        let t = resolve(&[], None, addr(), true, Some("box")).unwrap();
        assert_eq!(t.me.as_deref(), Some("http://127.0.0.1:3178"));
        assert_eq!(t.nodes, s(&["http://127.0.0.1:3178"]));
        // Web only, no nodes: empty router.
        let t = resolve(&[], None, addr(), false, None).unwrap();
        assert!(t.nodes.is_empty() && t.me.is_none());
        let t = resolve(&s(&[" "]), None, addr(), false, None).unwrap();
        assert!(t.nodes.is_empty());
    }

    #[test]
    fn identity_from_list_hostname_or_flag() {
        let nodes = s(&[
            "sourcegull-repo-0.sourcegull-repo:3178",
            "http://sourcegull-repo-1.sourcegull-repo:3178/",
        ]);
        let t = resolve(&nodes, None, addr(), true, Some("sourcegull-repo-1")).unwrap();
        assert_eq!(
            t.me.as_deref(),
            Some("http://sourcegull-repo-1.sourcegull-repo:3178")
        );
        assert_eq!(t.nodes[0], "http://sourcegull-repo-0.sourcegull-repo:3178");
        assert!(resolve(&nodes, None, addr(), true, Some("other")).is_err());
        assert!(resolve(&nodes, None, addr(), true, None).is_err());
        let t = resolve(
            &nodes,
            Some("sourcegull-repo-0.sourcegull-repo:3178"),
            addr(),
            true,
            None,
        )
        .unwrap();
        assert_eq!(
            t.me.as_deref(),
            Some("http://sourcegull-repo-0.sourcegull-repo:3178")
        );
        assert!(resolve(&nodes, Some("http://x:1"), addr(), true, None).is_err());
        let one = s(&["repo-0:3178"]);
        let t = resolve(&one, None, addr(), true, None).unwrap();
        assert_eq!(t.me.as_deref(), Some("http://repo-0:3178"));
    }

    #[test]
    fn hosts_are_parsed() {
        assert_eq!(host_of("http://a.b:3178"), "a.b");
        assert_eq!(host_of("http://a"), "a");
        assert_eq!(host_of("https://a:1/x"), "a");
    }
}
