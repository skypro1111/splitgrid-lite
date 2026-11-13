//! Postgres plans (`EXPLAIN (FORMAT JSON)`), read into a tree the Explain tab
//! draws: node, relation, estimated vs actual rows, cost and time, and which
//! nodes take most of the time.

use serde_json::Value as Json;

#[derive(Clone, Debug, PartialEq)]
pub struct PlanNode {
    pub node_type: String,
    /// "on users u", "using users_pkey", "Hash Join (Inner)"…
    pub detail: String,
    /// Filter / index condition / join condition, shown dim.
    pub condition: Option<String>,
    pub est_rows: f64,
    /// Per loop × loops, like EXPLAIN ANALYZE's text form multiplies.
    pub actual_rows: Option<f64>,
    pub total_cost: f64,
    /// Inclusive time in ms (all loops), with ANALYZE.
    pub total_ms: Option<f64>,
    /// This node alone: its time minus its children's.
    pub self_ms: Option<f64>,
    pub children: Vec<PlanNode>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub root: PlanNode,
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
}

fn num(v: &Json, key: &str) -> Option<f64> {
    v.get(key).and_then(Json::as_f64)
}

fn text(v: &Json, key: &str) -> Option<String> {
    v.get(key).and_then(Json::as_str).map(str::to_owned)
}

fn node(v: &Json) -> PlanNode {
    let node_type = text(v, "Node Type").unwrap_or_else(|| "?".into());
    let mut detail = Vec::new();
    if let Some(j) = text(v, "Join Type").filter(|_| node_type.contains("Join") || node_type == "Nested Loop") {
        detail.push(format!("({j})"));
    }
    if let Some(rel) = text(v, "Relation Name") {
        let schema = text(v, "Schema").map(|s| format!("{s}.")).unwrap_or_default();
        let alias = text(v, "Alias").filter(|a| *a != rel).map(|a| format!(" {a}")).unwrap_or_default();
        detail.push(format!("on {schema}{rel}{alias}"));
    } else if let Some(cte) = text(v, "CTE Name") {
        detail.push(format!("on {cte}"));
    }
    if let Some(ix) = text(v, "Index Name") {
        detail.push(format!("using {ix}"));
    }
    let condition = ["Index Cond", "Hash Cond", "Merge Cond", "Join Filter", "Filter", "Recheck Cond", "Sort Key"]
        .iter()
        .find_map(|k| match v.get(*k)? {
            Json::String(s) => Some(format!("{k}: {s}")),
            Json::Array(a) => Some(format!("{k}: {}", a.iter().filter_map(Json::as_str).collect::<Vec<_>>().join(", "))),
            _ => None,
        });
    let loops = num(v, "Actual Loops").unwrap_or(1.).max(1.);
    let children: Vec<PlanNode> = v.get("Plans").and_then(Json::as_array).map(|a| a.iter().map(node).collect()).unwrap_or_default();
    let total_ms = num(v, "Actual Total Time").map(|t| t * loops);
    // Init plans and CTEs can make children add up to more than the parent; never negative.
    let self_ms = total_ms.map(|t| (t - children.iter().filter_map(|c| c.total_ms).sum::<f64>()).max(0.));
    PlanNode {
        node_type,
        detail: detail.join(" "),
        condition,
        est_rows: num(v, "Plan Rows").unwrap_or(0.),
        actual_rows: num(v, "Actual Rows").map(|r| r * loops),
        total_cost: num(v, "Total Cost").unwrap_or(0.),
        total_ms,
        self_ms,
        children,
    }
}

/// A plan from the single cell `EXPLAIN (FORMAT JSON)` returns (JSON, or its text).
pub fn parse_postgres(cell: &sql_core::Value) -> Option<Plan> {
    let json = match cell {
        sql_core::Value::Json(j) => j.clone(),
        sql_core::Value::Text(s) | sql_core::Value::Other(s) => serde_json::from_str(s).ok()?,
        _ => return None,
    };
    let top = match &json {
        Json::Array(a) => a.first()?,
        obj => obj,
    };
    Some(Plan { root: node(top.get("Plan")?), planning_ms: num(top, "Planning Time"), execution_ms: num(top, "Execution Time") })
}

impl Plan {
    /// Depth-first, with each node's depth: the rows the tab draws.
    pub fn rows(&self) -> Vec<(usize, &PlanNode)> {
        fn walk<'a>(n: &'a PlanNode, depth: usize, out: &mut Vec<(usize, &'a PlanNode)>) {
            out.push((depth, n));
            for c in &n.children {
                walk(c, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        walk(&self.root, 0, &mut out);
        out
    }

    pub fn max_cost(&self) -> f64 {
        self.rows().iter().map(|(_, n)| n.total_cost).fold(0., f64::max)
    }

    /// The time the bars are scaled to: the whole query.
    pub fn total_ms(&self) -> Option<f64> {
        self.execution_ms.or(self.root.total_ms)
    }

    /// A node worth looking at: a third or more of the query's time on its own,
    /// or (without ANALYZE) most of the cost.
    pub fn is_slow(&self, n: &PlanNode) -> bool {
        match (n.self_ms, self.total_ms()) {
            (Some(own), Some(all)) if all > 0. => own / all >= 0.33,
            _ => {
                let own = n.total_cost - n.children.iter().map(|c| c.total_cost).sum::<f64>();
                let max = self.max_cost();
                max > 0. && own / max >= 0.5
            }
        }
    }

    /// Estimates off by 10× or more: where the planner guessed wrong.
    pub fn misestimated(n: &PlanNode) -> bool {
        n.actual_rows.is_some_and(|a| {
            let (a, e) = (a.max(1.), n.est_rows.max(1.));
            a / e >= 10. || e / a >= 10.
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANALYZED: &str = r#"[{"Plan":{"Node Type":"Hash Join","Join Type":"Inner","Total Cost":120.5,"Plan Rows":100,
        "Actual Rows":4000,"Actual Loops":1,"Actual Total Time":9.0,"Hash Cond":"(o.user_id = u.id)","Plans":[
          {"Node Type":"Seq Scan","Relation Name":"orders","Schema":"public","Alias":"o","Total Cost":80,"Plan Rows":4000,
           "Actual Rows":4000,"Actual Loops":1,"Actual Total Time":6.0,"Filter":"(total > 10)"},
          {"Node Type":"Hash","Total Cost":20,"Plan Rows":50,"Actual Rows":50,"Actual Loops":1,"Actual Total Time":1.0,"Plans":[
            {"Node Type":"Index Scan","Relation Name":"users","Alias":"users","Index Name":"users_pkey","Total Cost":18,
             "Plan Rows":50,"Actual Rows":25,"Actual Loops":2,"Actual Total Time":0.4}]}]},
        "Planning Time":0.2,"Execution Time":9.5}]"#;

    #[test]
    fn reads_an_analyzed_plan() {
        let plan = parse_postgres(&sql_core::Value::Text(ANALYZED.into())).unwrap();
        assert_eq!(plan.root.node_type, "Hash Join");
        assert_eq!(plan.root.detail, "(Inner)");
        assert_eq!(plan.root.condition.as_deref(), Some("Hash Cond: (o.user_id = u.id)"));
        assert_eq!(plan.execution_ms, Some(9.5));
        let rows = plan.rows();
        assert_eq!(rows.iter().map(|(d, n)| (*d, n.node_type.as_str())).collect::<Vec<_>>(), [(0, "Hash Join"), (1, "Seq Scan"), (1, "Hash"), (2, "Index Scan")]);
        let seq = rows[1].1;
        assert_eq!(seq.detail, "on public.orders o");
        let idx = rows[3].1;
        assert_eq!(idx.detail, "on users using users_pkey", "an alias equal to the name is left out");
        assert_eq!(idx.actual_rows, Some(50.), "rows × loops");
        assert_eq!(idx.total_ms, Some(0.8));
        assert_eq!(plan.root.self_ms, Some(2.0));
        assert!(plan.is_slow(seq), "6 of 9.5 ms");
        assert!(!plan.is_slow(idx));
        assert!(Plan::misestimated(&plan.root), "100 estimated, 4000 actual");
        assert!(!Plan::misestimated(seq));
        assert_eq!(plan.max_cost(), 120.5);
    }

    #[test]
    fn reads_a_plain_plan_from_json_and_rejects_garbage() {
        let json: Json = serde_json::from_str(r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"t","Total Cost":10,"Plan Rows":5}}]"#).unwrap();
        let plan = parse_postgres(&sql_core::Value::Json(json)).unwrap();
        assert_eq!(plan.root.total_ms, None);
        assert!(plan.is_slow(&plan.root), "all of the cost");
        assert!(parse_postgres(&sql_core::Value::Text("Seq Scan on t".into())).is_none());
        assert!(parse_postgres(&sql_core::Value::Int(1)).is_none());
    }
}
